//! Clarity / Scytl ENR (`results.enr.clarityelections.com/<ST>/<electionId>/...`).
//!
//! Clarity publishes immutable snapshots under a version number:
//! 1. `<base>/current_ver.txt` holds the current version (e.g. `356048`);
//! 2. `<base>/<ver>/reports/detailxml.zip` holds `detail.xml`: every contest, choice, vote type
//!    ("Election Day", "Absentee by Mail", "Early Voting", "Provisional", ...) and county;
//! 3. `<base>/<ver>/json/en/summary.json` (contest names `C`, key `K`, choices `CH`, parties `P`)
//!    plus `<base>/<ver>/json/details.json` (per contest `K`: counties `P`, votes `V[county][choice]`,
//!    totals `T[county]`) give the same county numbers without vote types.
//!
//! We poll only the version file and fetch a snapshot when the version changes. A version that
//! goes *down* is a regression (a rolled-back or mis-cached publish): we refuse it and report it.
//! `detail.xml` is preferred because the vote-type split drives the needle's mail/E-day skew; the
//! JSON pair is a fallback for elections that don't publish the zip.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Cursor, Read};

use async_trait::async_trait;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use serde::Deserialize;
use susq_contracts::v1::{CandidateVotes, VoteType};

use super::{AdapterContext, ParseError, Parsed, PollOutput, RawPayload, ResultsSource, SourceError};
use crate::http::{Fetched, HttpClient};
use crate::model::{classify_vote_type, normalize_party, parse_source_ts};

/// Which snapshot files to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarityDetail {
    /// `reports/detailxml.zip` (county × vote type).
    #[default]
    Xml,
    /// `json/en/summary.json` + `json/details.json` (county totals only, tagged MIXED).
    Json,
}

/// Polls one Clarity election.
pub struct ClaritySource {
    ctx: AdapterContext,
    base_url: String,
    detail: ClarityDetail,
    http: HttpClient,
    last_version: Option<u64>,
}

impl ClaritySource {
    pub fn new(ctx: AdapterContext, base_url: String, detail: ClarityDetail, http: HttpClient) -> Self {
        Self {
            ctx,
            base_url: base_url.trim_end_matches('/').to_owned(),
            detail,
            http,
            last_version: None,
        }
    }

    async fn fetch(&mut self, url: &str) -> Result<bytes::Bytes, SourceError> {
        // Snapshot URLs are versioned and immutable, so a conditional GET buys nothing.
        self.http.invalidate(url);
        match self.http.get(url).await {
            Ok(Fetched::Body(b)) => Ok(b),
            Ok(Fetched::NotModified) => Err(SourceError::Malformed {
                url: url.into(),
                reason: "unexpected 304".into(),
            }),
            Err(source) => Err(SourceError::Fetch {
                url: url.into(),
                source,
            }),
        }
    }
}

#[async_trait]
impl ResultsSource for ClaritySource {
    fn id(&self) -> &str {
        &self.ctx.source_id
    }

    async fn poll(&mut self) -> Result<PollOutput, SourceError> {
        let ver_url = format!("{}/current_ver.txt", self.base_url);
        let body = self.fetch(&ver_url).await?;
        let version = parse_version(&body).ok_or_else(|| SourceError::Malformed {
            url: ver_url.clone(),
            reason: "current_ver.txt is not a number".into(),
        })?;
        match self.last_version {
            Some(prev) if version < prev => {
                return Err(SourceError::VersionRegression { prev, got: version });
            }
            Some(prev) if version == prev => {
                return Ok(PollOutput {
                    not_modified: 1,
                    ..PollOutput::default()
                });
            }
            _ => {}
        }

        let observed = crate::model::now_ns();
        let mut out = PollOutput::default();
        let parsed = match self.detail {
            ClarityDetail::Xml => {
                let url = format!("{}/{version}/reports/detailxml.zip", self.base_url);
                let zip = self.fetch(&url).await?;
                out.raw.push(RawPayload {
                    url,
                    body: zip.clone(),
                });
                unzip_detail(&zip).and_then(|xml| parse_detail_xml(&xml, &self.ctx, observed))
            }
            ClarityDetail::Json => {
                let sum_url = format!("{}/{version}/json/en/summary.json", self.base_url);
                let det_url = format!("{}/{version}/json/details.json", self.base_url);
                let summary = self.fetch(&sum_url).await?;
                let details = self.fetch(&det_url).await?;
                out.raw.push(RawPayload {
                    url: sum_url,
                    body: summary.clone(),
                });
                out.raw.push(RawPayload {
                    url: det_url,
                    body: details.clone(),
                });
                parse_json(&summary, &details, &self.ctx, observed)
            }
        };
        match parsed {
            Ok(p) => {
                out.results = p.results;
                out.unresolved_counties = p.unresolved_counties;
                // Only a fully parsed snapshot advances the version; a bad one is retried.
                self.last_version = Some(version);
            }
            Err(e) => {
                out.malformed_payloads += 1;
                out.partial_errors.push(format!("version {version}: {e}"));
            }
        }
        Ok(out)
    }
}

/// `current_ver.txt` → version number.
pub fn parse_version(body: &[u8]) -> Option<u64> {
    std::str::from_utf8(body).ok()?.trim().parse().ok()
}

/// Extract the first `.xml` entry of `detailxml.zip`.
pub fn unzip_detail(zip_bytes: &[u8]) -> Result<String, ParseError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| ParseError(format!("zip: {e}")))?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| ParseError(format!("zip: {e}")))?;
        let is_xml = f
            .name()
            .map_err(|e| ParseError(format!("zip entry name: {e}")))?
            .to_ascii_lowercase()
            .ends_with(".xml");
        if is_xml {
            let mut s = String::new();
            f.read_to_string(&mut s)
                .map_err(|e| ParseError(format!("zip entry: {e}")))?;
            return Ok(s);
        }
    }
    Err(ParseError("zip has no .xml entry".into()))
}

/// One contest being accumulated from `detail.xml`.
#[derive(Default)]
struct ContestAcc {
    race_id: String,
    precincts: HashMap<String, (u32, u32)>,
    /// county → classified vote type → choice name → votes
    votes: BTreeMap<String, BTreeMap<i32, Vec<CandidateVotes>>>,
    labels: BTreeSet<String>,
}

fn attr(e: &BytesStart<'_>, name: &str) -> Result<Option<String>, ParseError> {
    match e.try_get_attribute(name) {
        Ok(Some(a)) => a
            .normalized_value(XmlVersion::Implicit1_0)
            .map(|v| Some(v.into_owned()))
            .map_err(|err| ParseError(format!("attribute {name}: {err}"))),
        Ok(None) => Ok(None),
        Err(err) => Err(ParseError(format!("attribute {name}: {err}"))),
    }
}

fn num_attr(e: &BytesStart<'_>, name: &str) -> Result<u64, ParseError> {
    match attr(e, name)? {
        None => Ok(0),
        Some(s) => s
            .trim()
            .parse()
            .map_err(|_| ParseError(format!("{name}={s:?} is not a count"))),
    }
}

/// Parse a *state-level* `detail.xml` (counties under each vote type) into county results.
///
/// Contests missing from the race map are skipped. If a contest reports a single vote-type label
/// (WV labels everything "Election Day"), the source is not really splitting, so results are
/// tagged `MIXED` rather than trusting the label.
pub fn parse_detail_xml(xml: &str, ctx: &AdapterContext, observed_at_ns: i64) -> Result<Parsed, ParseError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Parsed::default();
    let mut stack: Vec<String> = Vec::new();
    let mut in_timestamp = false;
    let mut source_ts = 0i64;
    let mut contest: Option<ContestAcc> = None;
    let mut skipping_contest = false;
    let mut choice: Option<(String, String)> = None;
    let mut vote_label: Option<String> = None;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| ParseError(format!("xml at {}: {e}", reader.error_position())))?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                let name = e.name().as_ref().to_owned();
                handle_open(
                    &e,
                    &name,
                    &stack,
                    ctx,
                    &mut contest,
                    &mut skipping_contest,
                    &mut choice,
                    &mut vote_label,
                )?;
                in_timestamp = name == "Timestamp";
                stack.push(name);
            }
            Event::Empty(e) => {
                let name = e.name().as_ref().to_owned();
                handle_open(
                    &e,
                    &name,
                    &stack,
                    ctx,
                    &mut contest,
                    &mut skipping_contest,
                    &mut choice,
                    &mut vote_label,
                )?;
            }
            Event::Text(t) if in_timestamp => {
                source_ts = parse_clarity_timestamp(&t.xml_content(XmlVersion::Implicit1_0), ctx.tz);
            }
            Event::End(e) => {
                let name = e.name().as_ref().to_owned();
                stack.pop();
                in_timestamp = false;
                match name.as_str() {
                    "Contest" => {
                        if let Some(acc) = contest.take() {
                            finish_contest(acc, ctx, source_ts, observed_at_ns, &mut out);
                        }
                        skipping_contest = false;
                    }
                    "Choice" => choice = None,
                    "VoteType" => vote_label = None,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(ParseError("truncated detail.xml".into()));
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn handle_open(
    e: &BytesStart<'_>,
    name: &str,
    stack: &[String],
    ctx: &AdapterContext,
    contest: &mut Option<ContestAcc>,
    skipping: &mut bool,
    choice: &mut Option<(String, String)>,
    vote_label: &mut Option<String>,
) -> Result<(), ParseError> {
    let parent = stack.last().map(String::as_str);
    match name {
        "Contest" => {
            let text = attr(e, "text")?.unwrap_or_default();
            match ctx.races.get(&text) {
                Some(race_id) => {
                    *contest = Some(ContestAcc {
                        race_id: race_id.to_owned(),
                        ..ContestAcc::default()
                    });
                    *skipping = false;
                }
                None => *skipping = true,
            }
        }
        _ if *skipping => {}
        "County" if parent == Some("ParticipatingCounties") => {
            if let Some(acc) = contest.as_mut() {
                let county = attr(e, "name")?.unwrap_or_default();
                let reported = u32::try_from(num_attr(e, "precinctsReported")?).unwrap_or(u32::MAX);
                let total = u32::try_from(num_attr(e, "precinctsParticipating")?).unwrap_or(u32::MAX);
                acc.precincts.insert(county, (reported, total));
            }
        }
        "Choice" => {
            let text = attr(e, "text")?.unwrap_or_default().trim().to_owned();
            let party = attr(e, "party")?.unwrap_or_default();
            *choice = Some((text, normalize_party(&party)));
        }
        "VoteType" => *vote_label = attr(e, "name")?,
        "County" if parent == Some("VoteType") => {
            let (Some(acc), Some((cand, party)), Some(label)) =
                (contest.as_mut(), choice.as_ref(), vote_label.as_ref())
            else {
                return Ok(());
            };
            let county = attr(e, "name")?.unwrap_or_default();
            let votes = num_attr(e, "votes")?;
            acc.labels.insert(label.clone());
            let vt = classify_vote_type(Some(label)) as i32;
            let list = acc.votes.entry(county).or_default().entry(vt).or_default();
            match list.iter_mut().find(|c| &c.candidate == cand) {
                Some(c) => c.votes += votes,
                None => list.push(CandidateVotes {
                    candidate: cand.clone(),
                    party: party.clone(),
                    votes,
                }),
            }
        }
        _ => {}
    }
    Ok(())
}

fn finish_contest(acc: ContestAcc, ctx: &AdapterContext, source_ts: i64, observed: i64, out: &mut Parsed) {
    let single_label = acc.labels.len() <= 1;
    for (county, by_type) in acc.votes {
        let Some(fips) = ctx.county_fips(&county) else {
            out.unresolved_counties += 1;
            continue;
        };
        let precincts = acc.precincts.get(&county).copied().unwrap_or((0, 0));
        if single_label {
            let merged: Vec<CandidateVotes> = by_type.into_values().flatten().collect();
            out.results.push(ctx.result(
                &acc.race_id,
                fips,
                VoteType::Mixed,
                merged,
                0,
                precincts,
                source_ts,
                observed,
            ));
            continue;
        }
        for (vt, votes) in by_type {
            let vt = VoteType::try_from(vt).unwrap_or(VoteType::Mixed);
            out.results.push(ctx.result(
                &acc.race_id,
                fips.clone(),
                vt,
                votes,
                0,
                precincts,
                source_ts,
                observed,
            ));
        }
    }
}

/// `12/18/2024 8:57:03 AM EST` → ns since epoch. US zone abbreviations are honoured; without one
/// the time is read in `fallback`. Returns 0 on failure.
pub fn parse_clarity_timestamp(s: &str, fallback: chrono_tz::Tz) -> i64 {
    let s = s.trim();
    let offset_h = |zone: &str| -> Option<i32> {
        Some(match zone {
            "UTC" | "GMT" => 0,
            "EDT" => -4,
            "EST" | "CDT" => -5,
            "CST" | "MDT" => -6,
            "MST" | "PDT" => -7,
            "PST" | "AKDT" => -8,
            "AKST" => -9,
            "HST" => -10,
            _ => return None,
        })
    };
    let Some((body, hours)) = s.rsplit_once(' ').and_then(|(b, z)| offset_h(z).map(|h| (b, h))) else {
        return parse_source_ts(s, fallback);
    };
    let Some(offset) = chrono::FixedOffset::east_opt(hours * 3600) else {
        return 0;
    };
    chrono::NaiveDateTime::parse_from_str(body, "%m/%d/%Y %I:%M:%S %p")
        .ok()
        .and_then(|n| n.and_local_timezone(offset).single())
        .and_then(|dt| dt.timestamp_nanos_opt())
        .unwrap_or(0)
}

#[derive(Debug, Deserialize)]
struct SummaryContest {
    #[serde(rename = "C")]
    name: String,
    #[serde(rename = "K")]
    key: String,
    #[serde(rename = "CH", default)]
    choices: Vec<String>,
    #[serde(rename = "P", default)]
    parties: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Details {
    #[serde(rename = "Contests")]
    contests: Vec<DetailContest>,
}

#[derive(Debug, Deserialize)]
struct DetailContest {
    #[serde(rename = "K")]
    key: String,
    #[serde(rename = "P", default)]
    counties: Vec<String>,
    #[serde(rename = "V", default)]
    votes: Vec<Vec<serde_json::Value>>,
}

/// Join `json/en/summary.json` (names) with `json/details.json` (county votes). All results are
/// `MIXED`. `details.V[i]` is aligned with `summary.CH` for the same contest key.
pub fn parse_json(
    summary: &[u8],
    details: &[u8],
    ctx: &AdapterContext,
    observed_at_ns: i64,
) -> Result<Parsed, ParseError> {
    let summary: Vec<SummaryContest> = serde_json::from_slice(summary)?;
    let details: Details = serde_json::from_slice(details)?;
    let by_key: HashMap<&str, &SummaryContest> = summary.iter().map(|c| (c.key.as_str(), c)).collect();
    let mut out = Parsed::default();
    for d in &details.contests {
        let Some(sc) = by_key.get(d.key.as_str()) else {
            continue;
        };
        let Some(race_id) = ctx.races.get(&sc.name) else {
            continue;
        };
        if d.votes.len() != d.counties.len() {
            return Err(ParseError(format!(
                "contest {}: {} counties but {} vote rows",
                d.key,
                d.counties.len(),
                d.votes.len()
            )));
        }
        for (county, row) in d.counties.iter().zip(&d.votes) {
            if row.len() != sc.choices.len() {
                return Err(ParseError(format!(
                    "contest {} county {county}: {} votes for {} choices",
                    d.key,
                    row.len(),
                    sc.choices.len()
                )));
            }
            let Some(fips) = ctx.county_fips(county) else {
                out.unresolved_counties += 1;
                continue;
            };
            let mut votes = Vec::with_capacity(row.len());
            for (i, v) in row.iter().enumerate() {
                // Clarity writes "protected" for privacy-suppressed cells.
                let n = crate::model::parse_count(v)
                    .ok_or_else(|| ParseError(format!("contest {} county {county}: bad votes {v}", d.key)))?;
                let party = sc.parties.get(i).map(String::as_str).unwrap_or_default();
                votes.push(CandidateVotes {
                    candidate: sc.choices[i].trim().to_owned(),
                    party: normalize_party(party),
                    votes: n,
                });
            }
            out.results.push(ctx.result(
                race_id,
                fips,
                VoteType::Mixed,
                votes,
                0,
                (0, 0),
                0,
                observed_at_ns,
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        let utc = chrono_tz::Tz::UTC;
        assert_eq!(
            parse_clarity_timestamp("11/3/2026 8:02:03 PM EST", utc),
            1_793_754_123_000_000_000
        );
        assert_eq!(
            parse_clarity_timestamp("11/3/2026 7:02:03 PM CST", utc),
            1_793_754_123_000_000_000
        );
        assert_eq!(
            parse_clarity_timestamp("11/3/2026 8:02:03 PM", chrono_tz::Tz::America__New_York),
            1_793_754_123_000_000_000
        );
        assert_eq!(parse_clarity_timestamp("nonsense", utc), 0);
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version(b"356048\n"), Some(356_048));
        assert_eq!(parse_version(b"<html>"), None);
    }
}
