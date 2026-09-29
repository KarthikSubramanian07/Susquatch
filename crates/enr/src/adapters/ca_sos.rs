//! California Secretary of State election-night JSON.
//!
//! Two interchangeable URL families return the same record shape (verified 2026-09-28 against
//! the live test feed):
//! * bulk files, one per contest type, with statewide/districtwide rows *and* one row per
//!   county: `https://media.sos.ca.gov/media/{governor,us-rep,...}.json` (S3 + CloudFront,
//!   serves `ETag`/`Last-Modified`), and
//! * the REST API: `https://api.sos.ca.gov/returns/<contest>[/county/<name>]`, which returns a
//!   single object or an array.
//!
//! A record looks like
//! `{"raceTitle": "United States Representative District 13 - Merced County Results",
//!   "Reporting": "1.5% (335 of 23,095) precincts reporting",
//!   "ReportingTime": "September 28, 2026, 2:11 p.m.",
//!   "candidates": [{"Name": "...", "Party": "Dem", "Votes": "1,234", "Percent": "53.1", "incumbent": false}]}`.
//!
//! Only county rows are emitted (the county model needs county granularity). CA does not split
//! election-night results by vote type in this feed, so every result is tagged `MIXED`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use susq_contracts::v1::{CandidateVotes, VoteType};

use super::{AdapterContext, ParseError, Parsed, PollOutput, ResultsSource, SourceError, poll_url_list};
use crate::http::HttpClient;
use crate::model::{normalize_party, parse_count};

/// Polls a list of CA SoS JSON URLs.
pub struct CaSosSource {
    ctx: AdapterContext,
    urls: Vec<String>,
    http: HttpClient,
}

impl CaSosSource {
    pub fn new(ctx: AdapterContext, urls: Vec<String>, http: HttpClient) -> Self {
        Self { ctx, urls, http }
    }
}

#[async_trait]
impl ResultsSource for CaSosSource {
    fn id(&self) -> &str {
        &self.ctx.source_id
    }

    async fn poll(&mut self) -> Result<PollOutput, SourceError> {
        let ctx = &self.ctx;
        poll_url_list(&mut self.http, &self.urls, |body, observed| {
            parse(body, ctx, observed)
        })
        .await
    }
}

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(rename = "raceTitle")]
    race_title: String,
    #[serde(rename = "Reporting", default)]
    reporting: String,
    #[serde(rename = "ReportingTime", default)]
    reporting_time: String,
    candidates: Vec<Candidate>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Party", default)]
    party: String,
    #[serde(rename = "Votes")]
    votes: Value,
}

/// Parse a CA SoS payload (object or array of records).
pub fn parse(body: &[u8], ctx: &AdapterContext, observed_at_ns: i64) -> Result<Parsed, ParseError> {
    let value: Value = serde_json::from_slice(body)?;
    let records: Vec<Record> = match value {
        Value::Array(_) => serde_json::from_value(value)?,
        Value::Object(_) => vec![serde_json::from_value(value)?],
        _ => return Err(ParseError("expected an object or array of records".into())),
    };
    let mut out = Parsed::default();
    for rec in records {
        let Some((contest, scope)) = rec.race_title.rsplit_once(" - ") else {
            return Err(ParseError(format!("unexpected raceTitle {:?}", rec.race_title)));
        };
        let Some(county) = scope
            .trim()
            .strip_suffix(" Results")
            .and_then(|s| s.strip_suffix(" County"))
        else {
            continue; // Statewide / Districtwide rows.
        };
        let Some(race_id) = ctx.races.get(contest) else {
            continue;
        };
        let Some(fips) = ctx.county_fips(county) else {
            out.unresolved_counties += 1;
            continue;
        };
        let mut votes = Vec::with_capacity(rec.candidates.len());
        for c in rec.candidates {
            let v = parse_count(&c.votes).ok_or_else(|| {
                ParseError(format!(
                    "{}: bad votes {} for {}",
                    rec.race_title, c.votes, c.name
                ))
            })?;
            votes.push(CandidateVotes {
                candidate: c.name.trim().to_owned(),
                party: normalize_party(&c.party),
                votes: v,
            });
        }
        let precincts = parse_reporting(&rec.reporting).unwrap_or((0, 0));
        let ts = parse_reporting_time(&rec.reporting_time, ctx.tz);
        out.results.push(ctx.result(
            race_id,
            fips,
            VoteType::Mixed,
            votes,
            0,
            precincts,
            ts,
            observed_at_ns,
        ));
    }
    Ok(out)
}

/// `"1.5% (335 of 23,095) precincts reporting"` → `(335, 23095)`.
pub fn parse_reporting(s: &str) -> Option<(u32, u32)> {
    let inner = s.split_once('(')?.1.split_once(')')?.0;
    let (a, b) = inner.split_once(" of ")?;
    let num = |x: &str| x.trim().replace(',', "").parse::<u32>().ok();
    Some((num(a)?, num(b)?))
}

/// `"September 28, 2026, 2:11 p.m."` (AP style, wall clock in `tz`) → ns since epoch; 0 if
/// unparseable.
pub fn parse_reporting_time(s: &str, tz: chrono_tz::Tz) -> i64 {
    let normalized = s
        .trim()
        .replace("a.m.", "AM")
        .replace("p.m.", "PM")
        .replace("noon", "12:00 PM")
        .replace("midnight", "12:00 AM");
    chrono::NaiveDateTime::parse_from_str(&normalized, "%B %d, %Y, %I:%M %p")
        .map_or(0, |naive| crate::model::local_to_ns(naive, tz))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporting_strings() {
        assert_eq!(
            parse_reporting("1.5% (335 of 23,095) precincts reporting"),
            Some((335, 23_095))
        );
        assert_eq!(
            parse_reporting("0% (0 of 627) precincts reporting"),
            Some((0, 627))
        );
        assert_eq!(parse_reporting("no precincts"), None);
    }

    #[test]
    fn reporting_times_in_pacific() {
        let la = chrono_tz::Tz::America__Los_Angeles;
        // The live test feed's own stamp: 2:11 PM PDT on Sep 28 2026 = 21:11Z.
        assert_eq!(
            parse_reporting_time("September 28, 2026, 2:11 p.m.", la),
            1_790_629_860_000_000_000
        );
        // Election night is PST: 8:00 PM on Nov 3 = 04:00Z Nov 4.
        assert_eq!(
            parse_reporting_time("November 3, 2026, 8:00 p.m.", la),
            1_793_764_800_000_000_000
        );
        assert!(parse_reporting_time("November 4, 2026, noon", la) > 0);
        assert_eq!(parse_reporting_time("soon", la), 0);
    }
}
