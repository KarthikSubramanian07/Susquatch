//! Delimited-text results files (CSV, TSV), plain or inside a zip, mapped by column name.
//!
//! Some states publish election-night results as one flat file rather than JSON. North
//! Carolina's State Board of Elections is the model case (verified 2026-10-06):
//! `https://s3.amazonaws.com/dl.ncsbe.gov/ENRS/<YYYY_MM_DD>/results_pct_<YYYYMMDD>.zip` holds a
//! single tab-separated, CRLF file with one row per (precinct, contest, choice) and one column
//! per vote method:
//!
//! ```text
//! County  Election Date  Precinct  Contest Group ID  Contest Type  Contest Name  Choice  Choice Party
//! Vote For  Election Day  Early Voting  Absentee by Mail  Provisional  Total Votes  Real Precinct
//! ```
//!
//! The adapter is configured entirely by header names (`[sources.mapping]`, see [`CsvMapping`]):
//! * rows are filtered by contest first (unmapped contests are skipped without parsing numbers),
//!   then grouped by (contest, county) and summed over every row of that county, so precinct-level
//!   files become county results;
//! * vote methods come either from one column per method (`vote_type_columns`, each classified by
//!   its header with [`classify_vote_type`]) or from a single `votes` column plus an optional
//!   per-row `vote_type` label (long format); without either split the result is `MIXED`;
//! * precinct reporting comes from explicit `precincts_reporting`/`precincts_total` columns
//!   (maximum over the county's rows) or, for precinct-level files without them, from a proxy:
//!   `precinct` names the precinct column, the county's total is the number of distinct
//!   precincts in the contest, and a precinct counts as reporting once any of its rows has a
//!   vote. `precinct_flag` restricts the count to rows whose flag column is truthy (NC's
//!   `Real Precinct = Y` excludes the ABSENTEE / EARLY VOTING / PROVISIONAL / TRANSFER
//!   pseudo-precincts, which carry votes but are not polling places).
//!
//! A zip body (detected by its magic bytes, so the same source works for `.csv` and `.zip`) is
//! unpacked to its first file entry, or to the first entry whose name ends with `zip_entry`.
//! Text is UTF-8 (a byte-order mark is skipped) with a Latin-1 fallback.
//!
//! As with the other adapters, a missing header or an unparseable count fails the whole payload:
//! a half-read file is worse than none.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Cursor, Read};

use async_trait::async_trait;
use serde::Deserialize;
use susq_contracts::v1::{CandidateVotes, VoteType};

use super::{AdapterContext, ParseError, Parsed, PollOutput, ResultsSource, SourceError, poll_url_list};
use crate::http::HttpClient;
use crate::model::{classify_vote_type, normalize_party, parse_source_ts};

/// Column mapping for a delimited results file. Every field names a header (matched after
/// trimming, case-insensitively).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CsvMapping {
    /// Field separator: one character, or `"\t"` / `"tab"` for tab-separated files.
    #[serde(default = "default_delimiter")]
    pub delimiter: String,
    /// For zip bodies: use the first entry whose name ends with this (case-insensitive).
    #[serde(default)]
    pub zip_entry: Option<String>,
    /// Contest name, looked up in the race map.
    pub race: String,
    /// County name, 5-digit FIPS or 3-digit county code.
    pub county: String,
    pub candidate: String,
    #[serde(default)]
    pub party: Option<String>,
    /// Single vote column (long format, or files without a method split).
    #[serde(default)]
    pub votes: Option<String>,
    /// Per-row vote-method label for long-format files (used with `votes`).
    #[serde(default)]
    pub vote_type: Option<String>,
    /// One column per vote method (wide format); each header is classified as a vote type.
    #[serde(default)]
    pub vote_type_columns: Vec<String>,
    /// Precinct id column, for the distinct-precinct reporting proxy.
    #[serde(default)]
    pub precinct: Option<String>,
    /// Only rows whose value here is truthy (`Y`, `yes`, `true`, `1`) count as precincts.
    #[serde(default)]
    pub precinct_flag: Option<String>,
    #[serde(default)]
    pub precincts_reporting: Option<String>,
    #[serde(default)]
    pub precincts_total: Option<String>,
    /// Row timestamp column (RFC 3339 or `M/D/YYYY h:mm:ss AM`, in the source's `tz`).
    #[serde(default)]
    pub source_ts: Option<String>,
}

fn default_delimiter() -> String {
    ",".into()
}

/// Checked form of a [`CsvMapping`].
#[derive(Debug, Clone)]
pub struct CompiledCsv {
    delimiter: char,
    m: CsvMapping,
}

/// A mapping that can't work, reported at start-up.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("csv mapping: {0}")]
pub struct CsvMappingError(pub String);

impl CsvMapping {
    /// Check the mapping is self-consistent.
    pub fn compile(&self) -> Result<CompiledCsv, CsvMappingError> {
        let err = |s: &str| Err(CsvMappingError(s.into()));
        let delimiter = match self.delimiter.as_str() {
            "\t" | "\\t" | "tab" => '\t',
            d => {
                let mut chars = d.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c != '"' && c != '\n' && c != '\r' => c,
                    _ => return err("delimiter must be one character other than a quote or newline"),
                }
            }
        };
        let has_columns = !self.vote_type_columns.is_empty();
        match (&self.votes, has_columns) {
            (None, false) => return err("set votes or vote_type_columns"),
            (Some(_), true) => return err("votes and vote_type_columns are exclusive"),
            _ => {}
        }
        if self.vote_type.is_some() && has_columns {
            return err("vote_type labels rows of a single votes column; drop it with vote_type_columns");
        }
        if self.precinct_flag.is_some() && self.precinct.is_none() {
            return err("precinct_flag needs precinct");
        }
        if self.precinct.is_some() && (self.precincts_reporting.is_some() || self.precincts_total.is_some()) {
            return err("use either the precinct proxy or precincts_reporting/precincts_total");
        }
        let names = [
            Some(&self.race),
            Some(&self.county),
            Some(&self.candidate),
            self.party.as_ref(),
            self.votes.as_ref(),
            self.vote_type.as_ref(),
            self.precinct.as_ref(),
            self.precinct_flag.as_ref(),
            self.precincts_reporting.as_ref(),
            self.precincts_total.as_ref(),
            self.source_ts.as_ref(),
        ];
        if names
            .into_iter()
            .flatten()
            .chain(&self.vote_type_columns)
            .any(|n| n.trim().is_empty())
        {
            return err("empty column name");
        }
        Ok(CompiledCsv {
            delimiter,
            m: self.clone(),
        })
    }
}

/// Polls a fixed list of delimited files (plain or zipped).
pub struct CsvSource {
    ctx: AdapterContext,
    urls: Vec<String>,
    mapping: CompiledCsv,
    http: HttpClient,
}

impl CsvSource {
    pub fn new(ctx: AdapterContext, urls: Vec<String>, mapping: CompiledCsv, http: HttpClient) -> Self {
        Self {
            ctx,
            urls,
            mapping,
            http,
        }
    }
}

#[async_trait]
impl ResultsSource for CsvSource {
    fn id(&self) -> &str {
        &self.ctx.source_id
    }

    async fn poll(&mut self) -> Result<PollOutput, SourceError> {
        let (ctx, mapping) = (&self.ctx, &self.mapping);
        poll_url_list(&mut self.http, &self.urls, |body, observed| {
            parse(body, mapping, ctx, observed)
        })
        .await
    }
}

/// Resolved header positions.
struct Columns {
    race: usize,
    county: usize,
    candidate: usize,
    party: Option<usize>,
    votes: Option<usize>,
    vote_type: Option<usize>,
    /// (column, vote type it carries)
    typed: Vec<(usize, VoteType)>,
    precinct: Option<usize>,
    precinct_flag: Option<usize>,
    precincts_reporting: Option<usize>,
    precincts_total: Option<usize>,
    source_ts: Option<usize>,
    width: usize,
}

impl Columns {
    fn resolve(header: &[String], m: &CsvMapping) -> Result<Self, ParseError> {
        let index: HashMap<String, usize> = header
            .iter()
            .enumerate()
            .map(|(i, h)| (h.trim().to_ascii_lowercase(), i))
            .collect();
        let find = |name: &str| -> Result<usize, ParseError> {
            index
                .get(&name.trim().to_ascii_lowercase())
                .copied()
                .ok_or_else(|| ParseError(format!("header has no column {name:?}")))
        };
        let opt = |name: &Option<String>| name.as_deref().map(find).transpose();
        let typed = m
            .vote_type_columns
            .iter()
            .map(|c| Ok((find(c)?, classify_vote_type(Some(c)))))
            .collect::<Result<Vec<_>, ParseError>>()?;
        let mut cols = Self {
            race: find(&m.race)?,
            county: find(&m.county)?,
            candidate: find(&m.candidate)?,
            party: opt(&m.party)?,
            votes: opt(&m.votes)?,
            vote_type: opt(&m.vote_type)?,
            typed,
            precinct: opt(&m.precinct)?,
            precinct_flag: opt(&m.precinct_flag)?,
            precincts_reporting: opt(&m.precincts_reporting)?,
            precincts_total: opt(&m.precincts_total)?,
            source_ts: opt(&m.source_ts)?,
            width: 0,
        };
        cols.width = [
            Some(cols.race),
            Some(cols.county),
            Some(cols.candidate),
            cols.party,
            cols.votes,
            cols.vote_type,
            cols.precinct,
            cols.precinct_flag,
            cols.precincts_reporting,
            cols.precincts_total,
            cols.source_ts,
        ]
        .into_iter()
        .flatten()
        .chain(cols.typed.iter().map(|(i, _)| *i))
        .max()
        .unwrap_or(0)
            + 1;
        Ok(cols)
    }
}

/// Everything summed for one (race, county).
#[derive(Default)]
struct CountyAcc {
    /// vote type → candidates in first-seen order
    by_type: BTreeMap<i32, Vec<CandidateVotes>>,
    precincts: BTreeSet<String>,
    reporting: BTreeSet<String>,
    explicit_precincts: (u64, u64),
    source_ts: i64,
}

impl CountyAcc {
    fn add(&mut self, vt: VoteType, name: &str, party: &str, votes: u64) {
        let list = self.by_type.entry(vt as i32).or_default();
        match list.iter_mut().find(|c| c.candidate == name && c.party == party) {
            Some(c) => c.votes = c.votes.saturating_add(votes),
            None => list.push(CandidateVotes {
                candidate: name.to_owned(),
                party: party.to_owned(),
                votes,
            }),
        }
    }
}

/// Parse one payload (a delimited file, or a zip holding one).
pub fn parse(
    body: &[u8],
    m: &CompiledCsv,
    ctx: &AdapterContext,
    observed_at_ns: i64,
) -> Result<Parsed, ParseError> {
    let bytes = if body.starts_with(b"PK\x03\x04") {
        unzip_entry(body, m.m.zip_entry.as_deref())?
    } else {
        body.to_vec()
    };
    let text = decode(&bytes);
    let mut records = Records::new(&text, m.delimiter);
    let header = loop {
        match records.next_record()? {
            None => return Err(ParseError("empty file".into())),
            Some(r) if r.iter().all(|f| f.trim().is_empty()) => continue,
            Some(r) => break r,
        }
    };
    let cols = Columns::resolve(&header, &m.m)?;

    let mut acc: BTreeMap<(String, String), CountyAcc> = BTreeMap::new();
    let mut unresolved: BTreeSet<(String, String)> = BTreeSet::new();
    let mut line = 1usize;
    while let Some(rec) = records.next_record()? {
        line += 1;
        if rec.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        if rec.len() < cols.width {
            return Err(ParseError(format!(
                "record {line} has {} fields, need {}",
                rec.len(),
                cols.width
            )));
        }
        let contest = rec[cols.race].trim();
        let Some(race_id) = ctx.races.get(contest) else {
            continue;
        };
        let county_raw = rec[cols.county].trim();
        let Some(fips) = ctx.county_fips(county_raw) else {
            unresolved.insert((race_id.to_owned(), county_raw.to_owned()));
            continue;
        };
        let name = rec[cols.candidate].trim();
        if name.is_empty() {
            return Err(ParseError(format!(
                "record {line}: {contest}/{county_raw}: no candidate"
            )));
        }
        let party = normalize_party(cols.party.map_or("", |i| rec[i].as_str()));
        let count = |i: usize, what: &str| -> Result<u64, ParseError> {
            parse_cell_count(&rec[i]).ok_or_else(|| {
                ParseError(format!(
                    "record {line}: {contest}/{county_raw}/{name}: bad {what} {:?}",
                    rec[i]
                ))
            })
        };
        let a = acc.entry((race_id.to_owned(), fips)).or_default();
        let mut row_votes = 0u64;
        if let Some(i) = cols.votes {
            let v = count(i, "votes")?;
            let vt = classify_vote_type(cols.vote_type.map(|j| rec[j].trim()));
            a.add(vt, name, &party, v);
            row_votes = v;
        }
        for &(i, vt) in &cols.typed {
            let v = count(i, "votes")?;
            a.add(vt, name, &party, v);
            row_votes = row_votes.saturating_add(v);
        }
        if let Some(p) = cols.precinct {
            let counts = cols.precinct_flag.is_none_or(|f| truthy(&rec[f]));
            let precinct = rec[p].trim();
            if counts && !precinct.is_empty() {
                a.precincts.insert(precinct.to_owned());
                if row_votes > 0 {
                    a.reporting.insert(precinct.to_owned());
                }
            }
        }
        if let Some(i) = cols.precincts_reporting {
            a.explicit_precincts.0 = a.explicit_precincts.0.max(count(i, "precincts reporting")?);
        }
        if let Some(i) = cols.precincts_total {
            a.explicit_precincts.1 = a.explicit_precincts.1.max(count(i, "precincts total")?);
        }
        if let Some(i) = cols.source_ts {
            a.source_ts = a.source_ts.max(parse_source_ts(&rec[i], ctx.tz));
        }
    }

    let mut out = Parsed {
        unresolved_counties: u32::try_from(unresolved.len()).unwrap_or(u32::MAX),
        ..Parsed::default()
    };
    let clamp = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
    for ((race_id, fips), a) in acc {
        let precincts = if cols.precinct.is_some() {
            (clamp(a.reporting.len() as u64), clamp(a.precincts.len() as u64))
        } else {
            (clamp(a.explicit_precincts.0), clamp(a.explicit_precincts.1))
        };
        for (vt, votes) in a.by_type {
            let vt = VoteType::try_from(vt).unwrap_or(VoteType::Mixed);
            out.results.push(ctx.result(
                &race_id,
                fips.clone(),
                vt,
                votes,
                0,
                precincts,
                a.source_ts,
                observed_at_ns,
            ));
        }
    }
    Ok(out)
}

/// A count cell: digits with optional thousands separators; an empty cell is zero.
fn parse_cell_count(s: &str) -> Option<u64> {
    let cleaned: String = s.trim().chars().filter(|c| *c != ',' && *c != '_').collect();
    if cleaned.is_empty() {
        return Some(0);
    }
    cleaned.parse::<u64>().ok()
}

fn truthy(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "true" | "t" | "1"
    )
}

/// The file inside a zip body: the first entry ending with `suffix`, else the first file.
pub fn unzip_entry(zip_bytes: &[u8], suffix: Option<&str>) -> Result<Vec<u8>, ParseError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| ParseError(format!("zip: {e}")))?;
    let suffix = suffix.map(str::to_ascii_lowercase);
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| ParseError(format!("zip: {e}")))?;
        if f.is_dir() {
            continue;
        }
        if suffix
            .as_deref()
            .is_some_and(|s| !f.name().to_ascii_lowercase().ends_with(s))
        {
            continue;
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .map_err(|e| ParseError(format!("zip entry {}: {e}", f.name())))?;
        return Ok(buf);
    }
    Err(ParseError(match suffix {
        Some(s) => format!("zip has no entry ending with {s:?}"),
        None => "zip has no file entry".into(),
    }))
}

/// UTF-8 (BOM skipped) with a Latin-1 fallback, so a stray accented byte never fails a payload.
fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

/// Streaming RFC 4180 reader: quoted fields (with `""` escapes and embedded delimiters or
/// newlines), CRLF or LF line ends, any single-character delimiter.
struct Records<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    delimiter: char,
}

impl<'a> Records<'a> {
    fn new(text: &'a str, delimiter: char) -> Self {
        Self {
            chars: text.chars().peekable(),
            delimiter,
        }
    }

    fn next_record(&mut self) -> Result<Option<Vec<String>>, ParseError> {
        if self.chars.peek().is_none() {
            return Ok(None);
        }
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut in_quotes = false;
        let mut quoted = false;
        loop {
            let Some(c) = self.chars.next() else {
                if in_quotes {
                    return Err(ParseError("unterminated quoted field".into()));
                }
                fields.push(field);
                return Ok(Some(fields));
            };
            if in_quotes {
                if c == '"' {
                    if self.chars.peek() == Some(&'"') {
                        self.chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    field.push(c);
                }
                continue;
            }
            match c {
                '"' if field.is_empty() && !quoted => {
                    in_quotes = true;
                    quoted = true;
                }
                c if c == self.delimiter => {
                    fields.push(std::mem::take(&mut field));
                    quoted = false;
                }
                '\r' if self.chars.peek() == Some(&'\n') => {}
                '\n' => {
                    fields.push(field);
                    return Ok(Some(fields));
                }
                c => field.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RaceMap;

    fn ctx() -> AdapterContext {
        AdapterContext {
            source_id: "nc_sbe".into(),
            state: "NC".into(),
            races: RaceMap::from_pairs([("US SENATE", "2026-SEN-NC")]),
            tz: chrono_tz::Tz::America__New_York,
        }
    }

    fn long_mapping() -> CsvMapping {
        CsvMapping {
            delimiter: ",".into(),
            zip_entry: None,
            race: "contest".into(),
            county: "county".into(),
            candidate: "name".into(),
            party: Some("party".into()),
            votes: Some("votes".into()),
            vote_type: Some("method".into()),
            vote_type_columns: Vec::new(),
            precinct: None,
            precinct_flag: None,
            precincts_reporting: Some("rep".into()),
            precincts_total: Some("tot".into()),
            source_ts: Some("updated".into()),
        }
    }

    #[test]
    fn records_handle_quotes_crlf_and_embedded_delimiters() {
        let text = "a,\"b,c\",\"say \"\"hi\"\"\"\r\n1,\"multi\nline\",3\n\nlast,,";
        let mut r = Records::new(text, ',');
        assert_eq!(
            r.next_record().expect("ok"),
            Some(vec!["a".into(), "b,c".into(), "say \"hi\"".into()])
        );
        assert_eq!(
            r.next_record().expect("ok"),
            Some(vec!["1".into(), "multi\nline".into(), "3".into()])
        );
        assert_eq!(r.next_record().expect("ok"), Some(vec![String::new()]));
        assert_eq!(
            r.next_record().expect("ok"),
            Some(vec!["last".into(), String::new(), String::new()])
        );
        assert_eq!(r.next_record().expect("ok"), None);
        assert!(Records::new("\"open", ',').next_record().is_err());
    }

    #[test]
    fn long_format_sums_rows_per_county_and_type() {
        let body = b"contest,county,name,party,method,votes,rep,tot,updated\n\
            US Senate,Wake,Cooper,DEM,Election Day,\"1,000\",10,200,11/3/2026 8:30:00 PM\n\
            US Senate,Wake,Cooper,DEM,Election Day,500,12,200,11/3/2026 8:45:00 PM\n\
            US Senate,Wake,Whatley,REP,Absentee by Mail,700,12,200,\n\
            US Senate,Atlantis,Cooper,DEM,Election Day,1,0,1,\n\
            Dog Catcher,Wake,Rex,,Election Day,lots,0,0,\n";
        let m = long_mapping().compile().expect("compile");
        let p = parse(body, &m, &ctx(), 9).expect("parse");
        assert_eq!(p.unresolved_counties, 1);
        assert_eq!(p.results.len(), 2, "E-day and mail for Wake");
        let eday = p
            .results
            .iter()
            .find(|r| r.vote_type == VoteType::ElectionDay as i32)
            .expect("eday");
        assert_eq!(eday.county_fips, "37183");
        assert_eq!(eday.votes[0].votes, 1_500);
        assert_eq!(eday.votes[0].party, "DEM");
        assert_eq!((eday.precincts_reporting, eday.precincts_total), (12, 200));
        // Latest row timestamp: 8:45 PM EST = 01:45Z Nov 4.
        assert_eq!(eday.source_ts_ns, 1_793_756_700_000_000_000);
        assert_eq!(eday.observed_at_ns, 9);
    }

    #[test]
    fn bad_payloads_error_instead_of_panicking() {
        let m = long_mapping().compile().expect("compile");
        assert!(parse(b"", &m, &ctx(), 0).is_err());
        assert!(
            parse(b"<html>blocked</html>", &m, &ctx(), 0).is_err(),
            "missing headers"
        );
        let bad = b"contest,county,name,party,method,votes,rep,tot,updated\nUS Senate,Wake,A,DEM,,-3,0,0,\n";
        assert!(parse(bad, &m, &ctx(), 0).is_err());
        let short = b"contest,county,name,party,method,votes,rep,tot,updated\nUS Senate,Wake\n";
        assert!(parse(short, &m, &ctx(), 0).is_err());
        assert!(parse(b"PK\x03\x04garbage", &m, &ctx(), 0).is_err());
    }

    #[test]
    fn compile_rejects_inconsistent_mappings() {
        let mut m = long_mapping();
        m.delimiter = "ab".into();
        assert!(m.compile().is_err());
        let mut m = long_mapping();
        m.votes = None;
        m.vote_type = None;
        assert!(m.compile().is_err(), "no vote column at all");
        let mut m = long_mapping();
        m.vote_type_columns = vec!["Election Day".into()];
        assert!(m.compile().is_err(), "votes and vote_type_columns together");
        let mut m = long_mapping();
        m.precinct_flag = Some("Real".into());
        assert!(m.compile().is_err(), "flag without precinct");
        let mut m = long_mapping();
        m.precinct = Some("Precinct".into());
        assert!(m.compile().is_err(), "proxy plus explicit precinct columns");
        let mut m = long_mapping();
        m.delimiter = "tab".into();
        assert_eq!(m.compile().expect("compile").delimiter, '\t');
    }
}
