//! Generic adapter: poll static JSON URLs and map them with a [`CompiledMapping`] from config.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;
use susq_contracts::v1::{CandidateVotes, CountyResult, VoteType};

use super::json_map::{CompiledMapping, Path, as_text, expand_rows, select, select_one};
use super::{
    AdapterContext, ParseError, Parsed, PollOutput, RawPayload, ResultsSource, SourceError, poll_url_list,
};
use crate::http::{Fetched, HttpClient};
use crate::model::{classify_vote_type, normalize_party, parse_count, parse_source_ts};

/// Polls a fixed list of JSON documents, optionally behind a version document.
pub struct StaticJsonSource {
    ctx: AdapterContext,
    urls: Vec<String>,
    mapping: CompiledMapping,
    http: HttpClient,
    version: Option<VersionedUrls>,
    last_version: Option<String>,
}

/// A version document whose value (at `path`) is substituted for `{version}` in the urls.
#[derive(Debug, Clone)]
pub struct VersionedUrls {
    pub url: String,
    pub path: Path,
}

impl StaticJsonSource {
    pub fn new(ctx: AdapterContext, urls: Vec<String>, mapping: CompiledMapping, http: HttpClient) -> Self {
        Self {
            ctx,
            urls,
            mapping,
            http,
            version: None,
            last_version: None,
        }
    }

    /// Resolve `{version}` in the urls from a version document before every poll.
    pub fn with_version(mut self, version: Option<VersionedUrls>) -> Self {
        self.version = version;
        self
    }

    /// Fetch the version document; returns the version and its fresh body (if not a 304).
    async fn current_version(
        &mut self,
        v: &VersionedUrls,
    ) -> Result<(String, Option<RawPayload>), SourceError> {
        let malformed = |reason: String| SourceError::Malformed {
            url: v.url.clone(),
            reason,
        };
        let body = match self.http.get(&v.url).await {
            Ok(Fetched::Body(b)) => b,
            Ok(Fetched::NotModified) => {
                return match &self.last_version {
                    Some(last) => Ok((last.clone(), None)),
                    None => {
                        self.http.invalidate(&v.url);
                        Err(malformed("304 before any version was read".into()))
                    }
                };
            }
            Err(source) => {
                return Err(SourceError::Fetch {
                    url: v.url.clone(),
                    source,
                });
            }
        };
        let parsed = parse_version(&body, &v.path);
        let version = match parsed {
            Ok(ver) => ver,
            Err(e) => {
                self.http.invalidate(&v.url);
                return Err(malformed(e.0));
            }
        };
        if let Some(last) = &self.last_version
            && let (Ok(prev), Ok(got)) = (last.parse::<u64>(), version.parse::<u64>())
            && got < prev
        {
            self.http.invalidate(&v.url);
            return Err(SourceError::VersionRegression { prev, got });
        }
        Ok((
            version,
            Some(RawPayload {
                url: v.url.clone(),
                body,
            }),
        ))
    }
}

/// The version value in a version document: a scalar at `path`, path-safe (letters, digits,
/// `.`, `_`, `-`) so it can only ever fill one url segment.
pub fn parse_version(body: &[u8], path: &Path) -> Result<String, ParseError> {
    let root: Value = serde_json::from_slice(body)?;
    let v = select_one(path, &root, &[], &root)
        .and_then(as_text)
        .map(|s| s.trim().to_owned())
        .ok_or_else(|| ParseError("version document has no scalar at the version path".into()))?;
    if v.is_empty()
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(ParseError(format!("version {v:?} is not path-safe")));
    }
    Ok(v)
}

#[async_trait]
impl ResultsSource for StaticJsonSource {
    fn id(&self) -> &str {
        &self.ctx.source_id
    }

    async fn poll(&mut self) -> Result<PollOutput, SourceError> {
        let (urls, version_raw) = match self.version.clone() {
            None => (self.urls.clone(), None),
            Some(v) => {
                let (ver, raw) = self.current_version(&v).await?;
                let urls = self.urls.iter().map(|u| u.replace("{version}", &ver)).collect();
                (urls, raw.map(|r| (ver, r)))
            }
        };
        let (ctx, mapping) = (&self.ctx, &self.mapping);
        let mut out = poll_url_list(&mut self.http, &urls, |body, observed| {
            parse(body, mapping, ctx, observed)
        })
        .await?;
        if let Some((ver, raw)) = version_raw {
            out.raw.insert(0, raw);
            // Advance only once the versioned documents parsed; a bad upload is retried.
            if out.malformed_payloads == 0 {
                self.last_version = Some(ver);
            } else if let Some(v) = &self.version {
                self.http.invalidate(&v.url);
            }
        }
        Ok(out)
    }
}

/// Parse one document. Rows for contests not in the race map are skipped silently; rows whose
/// county can't be resolved are counted; a row with an unparseable count fails the whole payload
/// (a half-read document is worse than none).
pub fn parse(
    body: &[u8],
    m: &CompiledMapping,
    ctx: &AdapterContext,
    observed_at_ns: i64,
) -> Result<Parsed, ParseError> {
    let root: Value = serde_json::from_slice(body)?;
    match &m.unwrap_base64 {
        None => parse_value(&root, m, ctx, observed_at_ns),
        Some(p) => {
            let encoded = select_one(p, &root, &[], &root)
                .and_then(Value::as_str)
                .ok_or_else(|| ParseError("unwrap_base64: no string at the configured path".into()))?;
            let decoded = decode_base64(encoded)?;
            let inner: Value = serde_json::from_slice(&decoded)?;
            parse_value(&inner, m, ctx, observed_at_ns)
        }
    }
}

/// Standard or URL-safe base64, padding optional, ASCII whitespace ignored.
pub fn decode_base64(s: &str) -> Result<Vec<u8>, ParseError> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b if b.is_ascii_whitespace() => continue,
            _ => return Err(ParseError(format!("base64: unexpected byte 0x{b:02x}"))),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// `"Gallego, Ruben (DEM)"` → `"DEM"`; `None` without a trailing parenthesised code.
fn party_suffix(name: &str) -> Option<&str> {
    let inner = name.trim_end().strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let code = inner[open + 1..].trim();
    (!code.is_empty()).then_some(code)
}

/// [`parse`] over an already-decoded document, for adapters that inspect the payload first
/// (e.g. [`super::enhanced_voting`] checks the ballot item and test flags before mapping).
pub fn parse_value(
    root: &Value,
    m: &CompiledMapping,
    ctx: &AdapterContext,
    observed_at_ns: i64,
) -> Result<Parsed, ParseError> {
    let mut out = Parsed::default();
    let doc_ts = m
        .source_ts
        .as_ref()
        .and_then(|p| select_one(p, root, &[], root))
        .and_then(as_text)
        .map_or(0, |s| parse_source_ts(&s, ctx.tz));

    for (row, binds) in expand_rows(root, &m.rows) {
        let text = |p: &super::json_map::Path| select_one(p, root, &binds, row).and_then(as_text);
        let Some(contest) = text(&m.race) else {
            return Err(ParseError("row without a contest name".into()));
        };
        let Some(race_id) = ctx.races.get(&contest) else {
            continue;
        };
        let Some(county_raw) = text(&m.county) else {
            return Err(ParseError(format!("{contest}: row without a county")));
        };
        let Some(fips) = ctx.county_fips(&county_raw) else {
            out.unresolved_counties += 1;
            continue;
        };
        let count = |p: &Option<super::json_map::Path>, what: &str| -> Result<u64, ParseError> {
            match p.as_ref().and_then(|p| select_one(p, root, &binds, row)) {
                None | Some(Value::Null) => Ok(0),
                Some(v) => parse_count(v)
                    .ok_or_else(|| ParseError(format!("{contest}/{county_raw}: bad {what} {v}"))),
            }
        };
        // One entry per vote type; without a breakdown there is exactly one (the row's type).
        let row_label = m
            .vote_type
            .as_ref()
            .and_then(|p| select_one(p, root, &binds, row))
            .and_then(as_text);
        let mut by_type: BTreeMap<i32, Vec<CandidateVotes>> = BTreeMap::new();
        for cand in select(&m.candidates, root, &binds, row) {
            let name = select_one(&m.candidate_name, root, &binds, cand)
                .and_then(as_text)
                .ok_or_else(|| ParseError(format!("{contest}/{county_raw}: candidate without a name")))?;
            let name = name.trim().to_owned();
            let party_raw = m
                .candidate_party
                .as_ref()
                .and_then(|p| select_one(p, root, &binds, cand))
                .and_then(as_text)
                .filter(|p| !p.trim().is_empty())
                .or_else(|| {
                    m.party_from_name
                        .then(|| party_suffix(&name).map(str::to_owned))
                        .flatten()
                })
                .unwrap_or_default();
            let party = normalize_party(&party_raw);
            let bad_votes = || ParseError(format!("{contest}/{county_raw}: bad votes for {name}"));
            let mut add = |vt: VoteType, v: u64| {
                let list = by_type.entry(vt as i32).or_default();
                match list.iter_mut().find(|c| c.candidate == name) {
                    Some(c) => c.votes += v,
                    None => list.push(CandidateVotes {
                        candidate: name.clone(),
                        party: party.clone(),
                        votes: v,
                    }),
                }
            };
            match &m.breakdown {
                None => {
                    let v = select_one(&m.candidate_votes, root, &binds, cand)
                        .and_then(parse_count)
                        .ok_or_else(bad_votes)?;
                    add(classify_vote_type(row_label.as_deref()), v);
                }
                Some((items, label, votes)) => {
                    let mut split = 0u64;
                    let mut unsplit = false;
                    for item in select(items, root, &binds, cand) {
                        let l = select_one(label, root, &binds, item).and_then(as_text);
                        match select_one(votes, root, &binds, item) {
                            None | Some(Value::Null) => unsplit = true,
                            Some(v) => {
                                let v = parse_count(v).ok_or_else(bad_votes)?;
                                split += v;
                                add(classify_vote_type(l.as_deref()), v);
                            }
                        }
                    }
                    // Some counties publish only the candidate total, with every method `null`
                    // (7 of 159 GA counties in the 2026 primary). The unsplit remainder of the
                    // candidate's total is MIXED rather than a parse failure or a silent zero.
                    if unsplit {
                        let total = select_one(&m.candidate_votes, root, &binds, cand)
                            .and_then(parse_count)
                            .ok_or_else(bad_votes)?;
                        let rest = total.checked_sub(split).ok_or_else(|| {
                            ParseError(format!(
                                "{contest}/{county_raw}: {name}'s methods exceed the total"
                            ))
                        })?;
                        if rest > 0 || split == 0 {
                            add(VoteType::Mixed, rest);
                        }
                    }
                }
            }
        }
        let total = if m.breakdown.is_some() {
            0
        } else {
            count(&m.total_votes, "total")?
        };
        let precincts = (
            u32::try_from(count(&m.precincts_reporting, "precincts reporting")?).unwrap_or(u32::MAX),
            u32::try_from(count(&m.precincts_total, "precincts total")?).unwrap_or(u32::MAX),
        );
        for (vt, votes) in by_type {
            let vt = VoteType::try_from(vt).unwrap_or(VoteType::Mixed);
            out.results.push(ctx.result(
                race_id,
                fips.clone(),
                vt,
                votes,
                total,
                precincts,
                doc_ts,
                observed_at_ns,
            ));
        }
    }
    if m.merge_rows {
        out.results = merge_results(std::mem::take(&mut out.results));
    }
    Ok(out)
}

/// Sum results that share (race, county, vote type): candidates by (name, party), precincts and
/// timestamps by maximum. A row's total may repeat the contest total (Arizona's
/// `ContestTotalVotes`), so merged totals are cleared and validation recomputes them from the
/// candidates. Output order follows first appearance.
fn merge_results(results: Vec<CountyResult>) -> Vec<CountyResult> {
    let mut index: BTreeMap<(String, String, i32), usize> = BTreeMap::new();
    let mut out: Vec<CountyResult> = Vec::new();
    for r in results {
        let key = (r.race_id.clone(), r.county_fips.clone(), r.vote_type);
        let Some(&i) = index.get(&key) else {
            index.insert(key, out.len());
            out.push(r);
            continue;
        };
        let acc = &mut out[i];
        for c in r.votes {
            match acc
                .votes
                .iter_mut()
                .find(|a| a.candidate == c.candidate && a.party == c.party)
            {
                Some(a) => a.votes = a.votes.saturating_add(c.votes),
                None => acc.votes.push(c),
            }
        }
        acc.total_votes = 0;
        acc.precincts_reporting = acc.precincts_reporting.max(r.precincts_reporting);
        acc.precincts_total = acc.precincts_total.max(r.precincts_total);
        acc.source_ts_ns = acc.source_ts_ns.max(r.source_ts_ns);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::json_map::JsonMapping;
    use crate::config::RaceMap;

    fn ctx() -> AdapterContext {
        AdapterContext {
            source_id: "nh_sos".into(),
            state: "NH".into(),
            races: RaceMap::from_pairs([("United States Senator", "2026-SEN-NH")]),
            tz: chrono_tz::Tz::America__New_York,
        }
    }

    fn mapping() -> CompiledMapping {
        JsonMapping {
            rows: "contests[*].counties[*]".into(),
            race: "^0.title".into(),
            county: "county".into(),
            candidates: "results[*]".into(),
            candidate_name: "name".into(),
            candidate_party: Some("party".into()),
            candidate_votes: "votes".into(),
            vote_type: Some("mode".into()),
            candidate_breakdown: None,
            breakdown_label: None,
            breakdown_votes: None,
            total_votes: None,
            precincts_reporting: Some("reporting".into()),
            precincts_total: Some("precincts".into()),
            source_ts: Some("$.updated".into()),
            unwrap_base64: None,
            merge_rows: false,
            party_from_name: false,
        }
        .compile()
        .expect("compile")
    }

    #[test]
    fn maps_nested_document() {
        let body = br#"{"updated":"2026-11-04T01:00:00Z","contests":[
            {"title":"United States Senator","counties":[
              {"county":"Merrimack","mode":"Election Day","reporting":10,"precincts":27,
               "results":[{"name":"Pappas","party":"Democratic","votes":"12,000"},{"name":"Opponent","party":"Republican","votes":9000}]},
              {"county":"Atlantis","results":[{"name":"Pappas","votes":1}]}
            ]},
            {"title":"Dog catcher","counties":[{"county":"Merrimack","results":[]}]}
        ]}"#;
        let parsed = parse(body, &mapping(), &ctx(), 7).expect("parse");
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.unresolved_counties, 1);
        let r = &parsed.results[0];
        assert_eq!(r.race_id, "2026-SEN-NH");
        assert_eq!(r.county_fips, "33013");
        assert_eq!(r.vote_type, VoteType::ElectionDay as i32);
        assert_eq!(r.votes[0].votes, 12_000);
        assert_eq!(r.votes[0].party, "DEM");
        assert_eq!((r.precincts_reporting, r.precincts_total), (10, 27));
        assert_eq!(r.observed_at_ns, 7);
        assert!(r.source_ts_ns > 0);
    }

    #[test]
    fn malformed_documents_error_instead_of_panicking() {
        let m = mapping();
        assert!(parse(b"not json", &m, &ctx(), 0).is_err());
        let bad_votes = br#"{"contests":[{"title":"United States Senator","counties":[
            {"county":"Merrimack","results":[{"name":"Pappas","votes":"lots"}]}]}]}"#;
        assert!(parse(bad_votes, &m, &ctx(), 0).is_err());
        // Wrong shape entirely: no rows, no error, no results.
        let parsed = parse(br#"{"contests": 5}"#, &m, &ctx(), 0).expect("parse");
        assert!(parsed.results.is_empty());
    }

    #[test]
    fn unsplit_breakdown_remainder_is_mixed() {
        let m = JsonMapping {
            rows: "counties[*]".into(),
            race: "$.title".into(),
            county: "county".into(),
            candidates: "results[*]".into(),
            candidate_name: "name".into(),
            candidate_party: None,
            candidate_votes: "votes".into(),
            vote_type: None,
            candidate_breakdown: Some("groups[*]".into()),
            breakdown_label: Some("label".into()),
            breakdown_votes: Some("votes".into()),
            total_votes: None,
            precincts_reporting: None,
            precincts_total: None,
            source_ts: None,
            unwrap_base64: None,
            merge_rows: false,
            party_from_name: false,
        }
        .compile()
        .expect("compile");
        let doc = |a: &str, b: &str| {
            format!(
                r#"{{"title":"United States Senator","counties":[{{"county":"Merrimack","results":[
                {{"name":"Pappas","votes":100,"groups":[{{"label":"Election Day","votes":{a}}},{{"label":"Absentee","votes":{b}}}]}}]}}]}}"#
            )
        };
        let votes_of = |p: &Parsed, vt: VoteType| {
            p.results
                .iter()
                .find(|r| r.vote_type == vt as i32)
                .map(|r| r.votes[0].votes)
        };
        let all_null = parse(doc("null", "null").as_bytes(), &m, &ctx(), 0).expect("parse");
        assert_eq!(all_null.results.len(), 1);
        assert_eq!(votes_of(&all_null, VoteType::Mixed), Some(100));
        let partial = parse(doc("60", "null").as_bytes(), &m, &ctx(), 0).expect("parse");
        assert_eq!(votes_of(&partial, VoteType::ElectionDay), Some(60));
        assert_eq!(votes_of(&partial, VoteType::Mixed), Some(40));
        let full = parse(doc("60", "40").as_bytes(), &m, &ctx(), 0).expect("parse");
        assert_eq!(votes_of(&full, VoteType::Mixed), None);
        assert!(parse(doc("160", "null").as_bytes(), &m, &ctx(), 0).is_err());
        assert!(parse(doc("\"x\"", "null").as_bytes(), &m, &ctx(), 0).is_err());
    }
}
