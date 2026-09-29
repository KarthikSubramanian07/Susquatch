//! Enhanced Voting results sites (Georgia's `results.sos.ga.gov`) with automatic election
//! discovery, so the 2026 general is picked up the moment the Secretary of State publishes it.
//!
//! Election slugs are ad hoc (`GeneralPrimary51926`, `06162026GeneralPrimaryRunoff`) and
//! ballot-item ids are GUIDs minted per election, so neither can be configured ahead of time.
//! The source is configured by *what* to follow (jurisdiction, election date, contest names) and
//! resolves *where* at runtime:
//!
//! ```text
//!             ┌──────────────── every discovery_interval, or soon after a 404 / stale id ───────────────┐
//!             ▼                                                                                          │
//!  GET /jurisdictions/{j} ──► elections[] with electionDate == date? ──no──► AwaitingElection ─────────┤
//!             │ yes (one or more, e.g. a primary and a recount on the same day)                          │
//!             ▼                                                                                          │
//!  GET /elections/{j}/{id}/data for each ──► empty (204)? ──► AwaitingData ────────────────────────────┤
//!             │ pick the election whose ballotItems[] match the most configured contests                 │
//!             ▼                                                                                          │
//!  match contest names (case/whitespace-insensitive) ──► none? ──► NoMatchingContests ─────────────────┤
//!             │ unmatched contests are logged and skipped                                                │
//!             ▼                                                                                          │
//!  Polling: GET /elections/{j}/{id}/data/ballot-item/{itemId} (conditional GET, backoff via service)    │
//!           ──► mapped with the configured [`JsonMapping`](super::json_map::JsonMapping) ──► results     │
//!           404 / 204 / item missing or renamed ──► request rediscovery ─────────────────────────────────┘
//! ```
//!
//! Every ballot-item payload carries the election's `isProduction` / `isOfficialResults` flags.
//! They are published in [`DiscoveryStatus`] (per-source health), and `require_production`
//! withholds results until the site says the data is real, so test data is never mistaken for
//! returns.
//!
//! None of this is a published API: it is what the results site itself calls (verified
//! 2026-09-28; see `docs/ELECTION_NIGHT.md` §1.3).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::NaiveDate;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;

use super::json_map::CompiledMapping;
use super::{
    AdapterContext, ParseError, Parsed, PollOutput, RawPayload, ResultsSource, SourceError, static_json,
};
use crate::config::{ContestSpec, fold_contest};
use crate::http::{FetchError, Fetched, HttpClient};

/// Georgia's public results API root.
pub const DEFAULT_BASE_URL: &str = "https://results.sos.ga.gov/results/public/api";

/// Upper bound on how soon a 404 or stale ballot item may trigger another discovery
/// (discovery downloads the ~2 MB election document, so it must not run on every poll).
pub const MIN_REDISCOVERY_GAP: Duration = Duration::from_secs(60);

/// Resolved settings for one Enhanced Voting source.
#[derive(Debug, Clone)]
pub struct Settings {
    pub base_url: String,
    pub jurisdiction: String,
    pub election_date: NaiveDate,
    pub contests: Vec<ContestSpec>,
    /// Scheduled re-discovery cadence.
    pub discovery_interval: Duration,
    /// Minimum time between discoveries forced by a 404 or stale id.
    pub min_rediscovery_gap: Duration,
    /// Withhold results unless the election is flagged `isProduction`.
    pub require_production: bool,
}

/// Where discovery stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryPhase {
    /// No election on the configured date is listed yet (normal before the SoS publishes it).
    #[default]
    AwaitingElection,
    /// The election is listed but its data document is still empty.
    AwaitingData,
    /// The election has data but none of the configured contests are on it.
    NoMatchingContests,
    /// At least one contest resolved to a ballot item and is being polled.
    Polling,
}

/// Discovery snapshot published in the source's health entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiscoveryStatus {
    pub phase: DiscoveryPhase,
    pub election_date: String,
    /// Public election id (URL slug) being polled.
    pub election_id: Option<String>,
    pub election_name: Option<String>,
    /// The site's own flag: `false` means test data. `None` until a payload has said.
    pub is_production: Option<bool>,
    pub is_official_results: Option<bool>,
    /// `election.lastUpdated` as the site printed it.
    pub source_last_updated: Option<String>,
    /// True only when the latest payload was explicitly flagged `isProduction`. Anything else
    /// (test data, flag missing, nothing polled yet) must not be treated as real returns.
    pub production_verified: bool,
    /// Configured contest name → ballot-item id (`None` = not on the ballot / unmatched).
    pub contests: BTreeMap<String, Option<String>>,
    pub discoveries: u64,
    pub discovery_failures: u64,
    /// A contest's ballot-item id changed between discoveries.
    pub ballot_item_id_changes: u64,
    /// Discoveries requested early because a ballot item 404'd or went stale.
    pub forced_rediscoveries: u64,
    /// Ballot-item payloads not mapped because `require_production` and the data was not
    /// flagged production.
    pub withheld_test_payloads: u64,
    /// Last discovery attempt (ns since epoch; 0 = never).
    pub last_discovery_ns: i64,
}

/// An election listed for the configured date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElectionRef {
    pub public_id: String,
    pub name: String,
}

/// The `election` block's status flags (present in `/data` and every ballot-item payload).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElectionFlags {
    #[serde(default)]
    pub is_production: Option<bool>,
    #[serde(default)]
    pub is_official_results: Option<bool>,
    #[serde(default)]
    pub last_updated: Option<String>,
}

/// Contest names resolved against one election's ballot items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    pub flags: ElectionFlags,
    /// (contest as configured, ballot-item id), in config order.
    pub matched: Vec<(ContestSpec, String)>,
    /// Configured contest names with no ballot item of that name.
    pub unmatched: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalizedText {
    #[serde(default)]
    language_id: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// English text if present, else the first non-empty entry.
fn pick_text(texts: &[LocalizedText]) -> Option<&str> {
    let usable = || texts.iter().filter_map(|t| t.text.as_deref().map(|s| (t, s)));
    usable()
        .find(|(t, _)| t.language_id.as_deref() == Some("en"))
        .or_else(|| usable().next())
        .map(|(_, s)| s)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JurisdictionDoc {
    #[serde(default)]
    elections: Vec<ElectionEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ElectionEntry {
    #[serde(default)]
    public_election_id: Option<String>,
    #[serde(default)]
    election_date: Option<String>,
    #[serde(default)]
    name: Vec<LocalizedText>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ElectionDataDoc {
    #[serde(default)]
    election: Option<ElectionFlags>,
    #[serde(default)]
    ballot_items: Vec<BallotItemEntry>,
}

#[derive(Debug, Deserialize)]
struct BallotItemEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Vec<LocalizedText>,
}

/// Elections listed in a jurisdiction document for `date`, in listing order.
///
/// `electionDate` is compared on its first ten characters, so `2026-11-03T00:00:00` also matches.
pub fn elections_on(body: &[u8], date: NaiveDate) -> Result<Vec<ElectionRef>, ParseError> {
    let doc: JurisdictionDoc = serde_json::from_slice(body)?;
    Ok(doc
        .elections
        .into_iter()
        .filter(|e| {
            e.election_date
                .as_deref()
                .and_then(|d| d.get(..10))
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                == Some(date)
        })
        .filter_map(|e| {
            let public_id = e.public_election_id.filter(|id| !id.trim().is_empty())?;
            let name = pick_text(&e.name).unwrap_or_default().trim().to_owned();
            Some(ElectionRef { public_id, name })
        })
        .collect())
}

/// Match configured contests to the ballot items of an election `/data` document.
///
/// Names are compared case- and whitespace-insensitively (the same folding as the race map).
/// If several ballot items share a name, the first listed wins.
pub fn resolve_contests(body: &[u8], contests: &[ContestSpec]) -> Result<Resolution, ParseError> {
    let doc: ElectionDataDoc = serde_json::from_slice(body)?;
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    for item in &doc.ballot_items {
        let (Some(id), Some(name)) = (item.id.as_deref(), pick_text(&item.name)) else {
            continue;
        };
        by_name.entry(fold_contest(name)).or_insert_with(|| id.to_owned());
    }
    let mut res = Resolution {
        flags: doc.election.unwrap_or_default(),
        ..Resolution::default()
    };
    for c in contests {
        match by_name.get(&fold_contest(&c.name)) {
            Some(id) => res.matched.push((c.clone(), id.clone())),
            None => res.unmatched.push(c.name.clone()),
        }
    }
    Ok(res)
}

/// What a ballot-item payload turned out to be.
#[derive(Debug)]
pub enum ItemPayload {
    /// The expected ballot item, with the election's flags.
    Current { root: Value, flags: ElectionFlags },
    /// Empty body or a document without this ballot item: the id is stale.
    Stale(String),
}

/// Decode a ballot-item response and check it really is `ballot_item_id`.
///
/// The site answers an unknown ballot-item id with `200` and `ballotItemWithBreakdown: null`,
/// and an unknown election with `204` and no body; both mean "rediscover", not "no votes yet".
pub fn inspect_item(body: &[u8], ballot_item_id: &str) -> Result<ItemPayload, ParseError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(ItemPayload::Stale("empty body".into()));
    }
    let root: Value = serde_json::from_slice(body)?;
    let item = &root["ballotItemWithBreakdown"];
    if !item.is_object() {
        return Ok(ItemPayload::Stale("no ballotItemWithBreakdown".into()));
    }
    if let Some(got) = item["id"].as_str()
        && !got.eq_ignore_ascii_case(ballot_item_id)
    {
        return Ok(ItemPayload::Stale(format!("payload is ballot item {got}")));
    }
    let flags = ElectionFlags::deserialize(&root["election"]).unwrap_or_default();
    Ok(ItemPayload::Current { root, flags })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    contest: String,
    ballot_item_id: String,
    url: String,
}

/// Polls one Enhanced Voting election, discovering it and its ballot items at runtime.
pub struct EnhancedVotingSource {
    ctx: AdapterContext,
    settings: Settings,
    base: Url,
    mapping: CompiledMapping,
    http: HttpClient,
    status: DiscoveryStatus,
    items: Vec<Tracked>,
    /// Scheduled discovery time (`None` = due now).
    next_discovery: Option<Instant>,
    last_discovery: Option<Instant>,
    rediscover_requested: bool,
}

/// `base_url` cannot carry path segments.
#[derive(Debug, thiserror::Error)]
#[error("base_url {0:?} is not a usable URL")]
pub struct BadBaseUrl(pub String);

impl EnhancedVotingSource {
    /// `ctx.races` must map every configured contest name to its race id (the mapper looks the
    /// ballot item's printed name up there).
    pub fn new(
        ctx: AdapterContext,
        settings: Settings,
        mapping: CompiledMapping,
        http: HttpClient,
    ) -> Result<Self, BadBaseUrl> {
        let base = Url::parse(&settings.base_url)
            .ok()
            .filter(|u| !u.cannot_be_a_base())
            .ok_or_else(|| BadBaseUrl(settings.base_url.clone()))?;
        let status = DiscoveryStatus {
            election_date: settings.election_date.to_string(),
            contests: settings.contests.iter().map(|c| (c.name.clone(), None)).collect(),
            ..DiscoveryStatus::default()
        };
        Ok(Self {
            ctx,
            settings,
            base,
            mapping,
            http,
            status,
            items: Vec::new(),
            next_discovery: None,
            last_discovery: None,
            rediscover_requested: false,
        })
    }

    /// Current discovery snapshot.
    pub fn status(&self) -> &DiscoveryStatus {
        &self.status
    }

    fn url(&self, tail: &[&str]) -> String {
        let mut u = self.base.clone();
        if let Ok(mut p) = u.path_segments_mut() {
            p.pop_if_empty().extend(tail);
        }
        u.into()
    }

    fn discovery_due(&self, now: Instant) -> bool {
        let scheduled = self.next_discovery.is_none_or(|t| now >= t);
        let forced = self.rediscover_requested
            && self
                .last_discovery
                .is_none_or(|t| now.duration_since(t) >= self.settings.min_rediscovery_gap);
        scheduled || forced
    }

    fn request_rediscovery(&mut self, url: &str, why: &str) {
        self.http.invalidate(url);
        if !self.rediscover_requested {
            self.rediscover_requested = true;
            self.status.forced_rediscoveries += 1;
            tracing::warn!(source = %self.ctx.source_id, %url, %why, "ballot item gone; rediscovering");
        }
    }

    /// GET a discovery document in full (no conditional GET: the body is always needed).
    async fn fetch_full(&mut self, url: &str) -> Result<Bytes, SourceError> {
        self.http.invalidate(url);
        let fetched = self.http.get(url).await;
        self.http.invalidate(url);
        match fetched {
            Ok(Fetched::Body(b)) => Ok(b),
            Ok(Fetched::NotModified) => Err(SourceError::Malformed {
                url: url.to_owned(),
                reason: "304 to an unconditional request".into(),
            }),
            Err(source) => Err(SourceError::Fetch {
                url: url.to_owned(),
                source,
            }),
        }
    }

    /// One discovery pass. On error the previous resolution (if any) stays in place.
    async fn discover(&mut self) -> Result<(), SourceError> {
        let now = Instant::now();
        self.next_discovery = Some(now + self.settings.discovery_interval);
        self.last_discovery = Some(now);
        self.rediscover_requested = false;
        self.status.discoveries += 1;
        self.status.last_discovery_ns = crate::model::now_ns();

        let list_url = self.url(&["jurisdictions", &self.settings.jurisdiction]);
        let body = self.fetch_full(&list_url).await?;
        let candidates =
            elections_on(&body, self.settings.election_date).map_err(|e| SourceError::Malformed {
                url: list_url.clone(),
                reason: e.0,
            })?;
        if candidates.is_empty() {
            self.apply(DiscoveryPhase::AwaitingElection, None, None);
            return Ok(());
        }

        // Several elections can share a date (a primary and a recount on 2026-05-19): follow the
        // one carrying the most configured contests; ties go to the first listed. One broken
        // candidate does not sink the others.
        let mut best: Option<(ElectionRef, Resolution)> = None;
        let mut last_error: Option<SourceError> = None;
        let mut empty = 0usize;
        for election in &candidates {
            let data_url = self.url(&[
                "elections",
                &self.settings.jurisdiction,
                &election.public_id,
                "data",
            ]);
            let body = match self.fetch_full(&data_url).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(source = %self.ctx.source_id, election = %election.public_id,
                        error = %e, "election data unavailable");
                    last_error = Some(e);
                    continue;
                }
            };
            if body.iter().all(u8::is_ascii_whitespace) {
                empty += 1;
                continue;
            }
            let res = match resolve_contests(&body, &self.settings.contests) {
                Ok(r) => r,
                Err(e) => {
                    last_error = Some(SourceError::Malformed {
                        url: data_url,
                        reason: e.0,
                    });
                    continue;
                }
            };
            if best
                .as_ref()
                .is_none_or(|(_, b)| res.matched.len() > b.matched.len())
            {
                best = Some((election.clone(), res));
            }
        }
        match (best, last_error) {
            (None, Some(e)) if empty == 0 => return Err(e),
            (None, _) => self.apply(DiscoveryPhase::AwaitingData, Some(&candidates[0]), None),
            (Some((e, res)), _) if res.matched.is_empty() => {
                self.apply(DiscoveryPhase::NoMatchingContests, Some(&e), Some(res));
            }
            (Some((e, res)), _) => self.apply(DiscoveryPhase::Polling, Some(&e), Some(res)),
        }
        Ok(())
    }

    /// Install a discovery result, logging transitions, id changes and unmatched contests.
    fn apply(&mut self, phase: DiscoveryPhase, election: Option<&ElectionRef>, res: Option<Resolution>) {
        let id = self.ctx.source_id.clone();
        let prev_phase = self.status.phase;
        let prev_election = self.status.election_id.clone();
        let new_election = election.map(|e| e.public_id.clone());
        if self.status.discoveries == 1 || prev_phase != phase || prev_election != new_election {
            tracing::info!(source = %id, ?phase, election = ?new_election,
                date = %self.settings.election_date, "enhanced voting discovery");
        }
        self.status.phase = phase;
        self.status.election_id = new_election.clone();
        self.status.election_name = election.map(|e| e.name.clone());

        let mut contests: BTreeMap<String, Option<String>> = self
            .settings
            .contests
            .iter()
            .map(|c| (c.name.clone(), None))
            .collect();
        let mut items = Vec::new();
        if let (Some(e), Some(res)) = (election, res) {
            self.update_flags(&res.flags);
            for (c, item_id) in &res.matched {
                let before = self.status.contests.get(&c.name).cloned().flatten();
                if prev_election == new_election
                    && let Some(before) = before
                    && before != *item_id
                {
                    self.status.ballot_item_id_changes += 1;
                    tracing::warn!(source = %id, contest = %c.name, from = %before, to = %item_id,
                        "ballot item id changed");
                }
                contests.insert(c.name.clone(), Some(item_id.clone()));
                items.push(Tracked {
                    contest: c.name.clone(),
                    ballot_item_id: item_id.clone(),
                    url: self.url(&[
                        "elections",
                        &self.settings.jurisdiction,
                        &e.public_id,
                        "data",
                        "ballot-item",
                        item_id,
                    ]),
                });
            }
            for name in &res.unmatched {
                if self.status.contests.get(name).is_none_or(Option::is_some) || prev_election != new_election
                {
                    tracing::warn!(source = %id, contest = %name, election = %e.public_id,
                        "contest not on this election's ballot; skipping");
                }
            }
        } else {
            self.status.is_production = None;
            self.status.is_official_results = None;
            self.status.source_last_updated = None;
            self.status.production_verified = false;
        }
        self.status.contests = contests;
        self.items = items;
    }

    fn update_flags(&mut self, flags: &ElectionFlags) {
        let was = self.status.is_production;
        self.status.is_production = flags.is_production;
        self.status.is_official_results = flags.is_official_results;
        if flags.last_updated.is_some() {
            self.status.source_last_updated = flags.last_updated.clone();
        }
        self.status.production_verified = flags.is_production == Some(true);
        if was != flags.is_production && flags.is_production != Some(true) {
            tracing::warn!(source = %self.ctx.source_id, is_production = ?flags.is_production,
                withheld = self.settings.require_production,
                "election is not flagged production: results are TEST data");
        }
    }

    /// Poll every resolved ballot item once.
    async fn poll_items(&mut self, out: &mut PollOutput) -> Result<(), SourceError> {
        let mut failures = 0usize;
        let items = self.items.clone();
        for item in &items {
            match self.http.get(&item.url).await {
                Ok(Fetched::NotModified) => out.not_modified += 1,
                Ok(Fetched::Body(body)) => {
                    if !self.handle_item_body(item, &body, out) {
                        failures += 1;
                    }
                    out.raw.push(RawPayload {
                        url: item.url.clone(),
                        body,
                    });
                }
                Err(e) => {
                    if matches!(e, FetchError::Status(404 | 410)) {
                        self.request_rediscovery(&item.url, &e.to_string());
                    }
                    out.partial_errors.push(format!("{}: {e}", item.url));
                    failures += 1;
                }
            }
        }
        if !items.is_empty() && failures == items.len() && out.raw.is_empty() {
            return Err(SourceError::AllFailed(out.partial_errors.join("; ")));
        }
        Ok(())
    }

    /// Returns false when the body did not yield usable data (stale or malformed).
    fn handle_item_body(&mut self, item: &Tracked, body: &[u8], out: &mut PollOutput) -> bool {
        let observed = crate::model::now_ns();
        let payload = match inspect_item(body, &item.ballot_item_id) {
            Ok(p) => p,
            Err(e) => {
                self.http.invalidate(&item.url);
                out.malformed_payloads += 1;
                out.partial_errors.push(format!("{}: {e}", item.url));
                return false;
            }
        };
        let (root, flags) = match payload {
            ItemPayload::Current { root, flags } => (root, flags),
            ItemPayload::Stale(why) => {
                out.partial_errors.push(format!(
                    "{} ({}): stale ballot item: {why}",
                    item.url, item.contest
                ));
                self.request_rediscovery(&item.url, &why);
                return false;
            }
        };
        self.update_flags(&flags);
        if self.settings.require_production && flags.is_production != Some(true) {
            self.status.withheld_test_payloads += 1;
            // Forget validators so the production payload is fetched in full once it flips.
            self.http.invalidate(&item.url);
            return true;
        }
        match static_json::parse_value(&root, &self.mapping, &self.ctx, observed) {
            Ok(Parsed {
                results,
                unresolved_counties,
            }) => {
                out.results.extend(results);
                out.unresolved_counties += unresolved_counties;
                true
            }
            Err(e) => {
                self.http.invalidate(&item.url);
                out.malformed_payloads += 1;
                out.partial_errors.push(format!("{}: {e}", item.url));
                false
            }
        }
    }
}

#[async_trait]
impl ResultsSource for EnhancedVotingSource {
    fn id(&self) -> &str {
        &self.ctx.source_id
    }

    async fn poll(&mut self) -> Result<PollOutput, SourceError> {
        let mut out = PollOutput::default();
        if self.discovery_due(Instant::now())
            && let Err(e) = self.discover().await
        {
            self.status.discovery_failures += 1;
            tracing::warn!(source = %self.ctx.source_id, error = %e, "discovery failed");
            if self.items.is_empty() {
                return Err(e);
            }
            out.partial_errors.push(format!("discovery: {e}"));
        }
        let result = self.poll_items(&mut out).await;
        out.discovery = Some(Box::new(self.status.clone()));
        result.map(|()| out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, race: &str) -> ContestSpec {
        ContestSpec {
            name: name.into(),
            race_id: race.into(),
        }
    }

    #[test]
    fn election_dates_match_exactly_including_timestamped_forms() {
        let body = br#"{"elections":[
            {"publicElectionId":"A","electionDate":"2026-11-03","name":[{"languageId":"es","text":"General (es)"},{"languageId":"en","text":"General"}]},
            {"publicElectionId":"B","electionDate":"2026-11-03T00:00:00"},
            {"publicElectionId":"C","electionDate":"2026-12-01"},
            {"publicElectionId":"","electionDate":"2026-11-03"},
            {"electionDate":"2026-11-03"},
            {"publicElectionId":"D","electionDate":"garbage"}
        ]}"#;
        let date = NaiveDate::from_ymd_opt(2026, 11, 3).expect("date");
        let got = elections_on(body, date).expect("parse");
        assert_eq!(
            got,
            vec![
                ElectionRef {
                    public_id: "A".into(),
                    name: "General".into()
                },
                ElectionRef {
                    public_id: "B".into(),
                    name: String::new()
                },
            ]
        );
        assert!(elections_on(b"<html>", date).is_err());
        assert!(elections_on(b"{}", date).expect("no elections key").is_empty());
    }

    #[test]
    fn contest_names_fold_case_and_whitespace() {
        let body = br#"{"election":{"isProduction":false},"ballotItems":[
            {"id":"x1","name":[{"languageId":"en","text":"  US   SENATE "}]},
            {"id":"x2","name":[{"languageId":"en","text":"us senate"}]},
            {"id":"x3","name":[]},
            {"name":[{"text":"Governor"}]}
        ]}"#;
        let res = resolve_contests(
            body,
            &[spec("US Senate", "2026-SEN-GA"), spec("Governor", "2026-GOV-GA")],
        )
        .expect("parse");
        assert_eq!(
            res.matched,
            vec![(spec("US Senate", "2026-SEN-GA"), "x1".into())],
            "first of duplicate names wins; items without an id are ignored"
        );
        assert_eq!(res.unmatched, vec!["Governor".to_owned()]);
        assert_eq!(res.flags.is_production, Some(false));
    }

    #[test]
    fn stale_ballot_items_are_recognized() {
        assert!(matches!(inspect_item(b"", "a"), Ok(ItemPayload::Stale(_))));
        assert!(matches!(
            inspect_item(br#"{"ballotItemWithBreakdown":null}"#, "a"),
            Ok(ItemPayload::Stale(_))
        ));
        assert!(matches!(
            inspect_item(br#"{"ballotItemWithBreakdown":{"id":"b"}}"#, "a"),
            Ok(ItemPayload::Stale(_))
        ));
        assert!(inspect_item(b"{not json", "a").is_err());
        let Ok(ItemPayload::Current { flags, .. }) = inspect_item(
            br#"{"election":{"isProduction":true,"lastUpdated":"t"},"ballotItemWithBreakdown":{"id":"A"}}"#,
            "a",
        ) else {
            panic!("current payload expected");
        };
        assert_eq!(flags.is_production, Some(true));
        assert_eq!(flags.last_updated.as_deref(), Some("t"));
    }
}
