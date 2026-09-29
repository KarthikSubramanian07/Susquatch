//! Results-source adapters. Each turns one public results site into normalized, cumulative
//! [`CountyResult`]s; the [`crate::pipeline`] does dedupe, monotonicity and cross-checks.
//!
//! Adapters split *fetching* (async, stateful: validators, versions) from *parsing* (pure
//! functions over bytes), so every parser is tested directly against recorded fixtures.

pub mod ca_sos;
pub mod clarity;
pub mod enhanced_voting;
pub mod json_map;
pub mod static_json;

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use susq_contracts::v1::{CandidateVotes, CountyResult, VoteType};

use crate::config::{EnrConfig, RaceMap, SourceConfig, SourceKind};
use crate::fips::FipsIndex;
use crate::http::{FetchError, Fetched, HttpClient};

/// A raw response body kept for the capture archive.
#[derive(Debug, Clone)]
pub struct RawPayload {
    pub url: String,
    pub body: Bytes,
}

/// What one poll produced.
#[derive(Debug, Default)]
pub struct PollOutput {
    /// Normalized cumulative results (unvalidated; the pipeline validates).
    pub results: Vec<CountyResult>,
    /// Fresh bodies fetched this poll (not-modified responses are not repeated).
    pub raw: Vec<RawPayload>,
    /// URLs that answered 304 / an unchanged version.
    pub not_modified: u32,
    /// Payloads that were fetched but could not be parsed. Counted, never fatal.
    pub malformed_payloads: u32,
    /// Rows skipped because the county could not be resolved to a FIPS code.
    pub unresolved_counties: u32,
    /// Non-fatal errors for individual URLs (the poll still returned other data).
    pub partial_errors: Vec<String>,
    /// Discovery state of sources that find their endpoints at runtime (latest snapshot).
    pub discovery: Option<Box<enhanced_voting::DiscoveryStatus>>,
}

/// Why a whole poll failed.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("fetch {url}: {source}")]
    Fetch {
        url: String,
        #[source]
        source: FetchError,
    },
    #[error("malformed payload from {url}: {reason}")]
    Malformed { url: String, reason: String },
    #[error("version went backwards: {prev} -> {got}")]
    VersionRegression { prev: u64, got: u64 },
    #[error("every url failed: {0}")]
    AllFailed(String),
}

/// One public results source.
#[async_trait]
pub trait ResultsSource: Send {
    /// Stable id (matches `CountyResult.source`).
    fn id(&self) -> &str;
    /// Fetch whatever is new and return it normalized. Must never panic on bad input.
    async fn poll(&mut self) -> Result<PollOutput, SourceError>;
}

/// Everything a parser needs to know about its source.
#[derive(Debug, Clone)]
pub struct AdapterContext {
    pub source_id: String,
    pub state: String,
    pub races: RaceMap,
    /// Zone for source timestamps printed without one.
    pub tz: chrono_tz::Tz,
}

impl AdapterContext {
    /// Resolve a county name/code in this source's state.
    pub fn county_fips(&self, county: &str) -> Option<String> {
        FipsIndex::global().resolve(&self.state, county)
    }

    /// Assemble a result with this source's identity filled in.
    #[allow(clippy::too_many_arguments)]
    pub fn result(
        &self,
        race_id: &str,
        county_fips: String,
        vote_type: VoteType,
        votes: Vec<CandidateVotes>,
        total_votes: u64,
        precincts: (u32, u32),
        source_ts_ns: i64,
        observed_at_ns: i64,
    ) -> CountyResult {
        CountyResult {
            race_id: race_id.to_owned(),
            state: self.state.clone(),
            county_fips,
            source: self.source_id.clone(),
            source_ts_ns,
            observed_at_ns,
            vote_type: vote_type as i32,
            votes,
            total_votes,
            precincts_reporting: precincts.0,
            precincts_total: precincts.1,
            batch_id: String::new(),
            is_correction: false,
        }
    }
}

/// Parser output for one payload.
#[derive(Debug, Default)]
pub struct Parsed {
    pub results: Vec<CountyResult>,
    pub unresolved_counties: u32,
}

/// Error from a pure parser.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

impl From<serde_json::Error> for ParseError {
    fn from(e: serde_json::Error) -> Self {
        Self(format!("json: {e}"))
    }
}

/// Build the adapter for one configured source.
pub fn build_source(cfg: &EnrConfig, s: &SourceConfig) -> Result<Box<dyn ResultsSource>, BuildError> {
    let timing = cfg.timing(s);
    let http =
        HttpClient::new(timing.timeout, &cfg.defaults.user_agent).map_err(|e| BuildError(e.to_string()))?;
    let ctx = AdapterContext {
        source_id: s.id.clone(),
        state: s.state.clone(),
        races: cfg.race_map(&s.id),
        tz: s.tz,
    };
    Ok(match &s.kind {
        SourceKind::CaSos { urls } => Box::new(ca_sos::CaSosSource::new(ctx, urls.clone(), http)),
        SourceKind::Clarity { base_url, detail } => {
            Box::new(clarity::ClaritySource::new(ctx, base_url.clone(), *detail, http))
        }
        SourceKind::StaticJson { urls, mapping } => {
            let compiled = mapping
                .compile()
                .map_err(|e| BuildError(format!("{}: {e}", s.id)))?;
            Box::new(static_json::StaticJsonSource::new(
                ctx,
                urls.clone(),
                compiled,
                http,
            ))
        }
        SourceKind::EnhancedVoting {
            base_url,
            jurisdiction,
            election_date,
            contests,
            discovery_interval_s,
            require_production,
            mapping,
        } => {
            let compiled = mapping
                .compile()
                .map_err(|e| BuildError(format!("{}: {e}", s.id)))?;
            let mut ctx = ctx;
            for c in contests {
                ctx.races.insert(&c.name, &c.race_id);
            }
            let discovery_interval = Duration::from_secs_f64(discovery_interval_s.max(1.0));
            let settings = enhanced_voting::Settings {
                base_url: base_url.clone(),
                jurisdiction: jurisdiction.clone(),
                election_date: *election_date,
                contests: contests.clone(),
                discovery_interval,
                min_rediscovery_gap: discovery_interval.min(enhanced_voting::MIN_REDISCOVERY_GAP),
                require_production: *require_production,
            };
            Box::new(
                enhanced_voting::EnhancedVotingSource::new(ctx, settings, compiled, http)
                    .map_err(|e| BuildError(format!("{}: {e}", s.id)))?,
            )
        }
    })
}

/// A source could not be constructed from config.
#[derive(Debug, thiserror::Error)]
#[error("building source: {0}")]
pub struct BuildError(pub String);

/// Shared loop for sources that are "a list of static URLs, each parsed independently":
/// conditional GET each URL, parse fresh bodies, and tolerate individual failures.
///
/// A malformed body is counted and its validators dropped (so the next poll refetches in full);
/// the poll only fails when *every* URL failed to return a body.
pub(crate) async fn poll_url_list(
    http: &mut HttpClient,
    urls: &[String],
    mut parse: impl FnMut(&[u8], i64) -> Result<Parsed, ParseError>,
) -> Result<PollOutput, SourceError> {
    let mut out = PollOutput::default();
    let mut failures = 0usize;
    for url in urls {
        match http.get(url).await {
            Ok(Fetched::NotModified) => out.not_modified += 1,
            Ok(Fetched::Body(body)) => {
                let observed = crate::model::now_ns();
                match parse(&body, observed) {
                    Ok(parsed) => {
                        out.results.extend(parsed.results);
                        out.unresolved_counties += parsed.unresolved_counties;
                    }
                    Err(e) => {
                        http.invalidate(url);
                        out.malformed_payloads += 1;
                        out.partial_errors.push(format!("{url}: {e}"));
                        failures += 1;
                    }
                }
                out.raw.push(RawPayload {
                    url: url.clone(),
                    body,
                });
            }
            Err(e) => {
                out.partial_errors.push(format!("{url}: {e}"));
                failures += 1;
            }
        }
    }
    // Malformed bodies are still returned (and archived) rather than turned into an error.
    if !urls.is_empty() && failures == urls.len() && out.raw.is_empty() {
        return Err(SourceError::AllFailed(out.partial_errors.join("; ")));
    }
    Ok(out)
}
