//! Nevada Secretary of State results XML (`silverstateelection.nv.gov/_xml/USandNV.xml`).
//!
//! One document holds every federal and statewide race:
//!
//! ```xml
//! <USNVResults>
//!   <UIPage Title="USSenate">
//!     <Race RaceID="2" RaceTitle="United States Senator" PrecinctsReported="1641" TotalPrecincts="1641">
//!       <Candidate CandidateName="ROSEN, JACKY S." Party="DEM" Votes="701,105"
//!                  CarsonCityVotes="13,454" ClarkVotes="514,662" WhitePineVotes="..." />
//!     </Race>
//!     <RaceTopTwo ...><CandidateTopTwo .../></RaceTopTwo>
//!   </UIPage>
//! </USNVResults>
//! ```
//!
//! County votes are attributes named `<County>Votes` on each `Candidate` (camel case, so
//! `CarsonCity` is Carson City). `RaceTopTwo` repeats the leading candidates and is skipped.
//! Precincts are reported per race, not per county, and there is no vote-method split or
//! timestamp, so results are MIXED with zero precinct counts.
//!
//! The host sits behind an Imperva (Incapsula) JavaScript challenge, so the configured source is
//! disabled; the parser is tested on the 2024 general's final document (docs/ELECTION_NIGHT.md
//! §1.4). A challenge page is HTML and fails the parse, which health counts as malformed.

use std::collections::BTreeMap;

use async_trait::async_trait;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use susq_contracts::v1::{CandidateVotes, VoteType};

use super::{AdapterContext, ParseError, Parsed, PollOutput, ResultsSource, SourceError, poll_url_list};
use crate::http::HttpClient;
use crate::model::normalize_party;

/// Polls the Nevada SoS XML document(s).
pub struct NvSosSource {
    ctx: AdapterContext,
    urls: Vec<String>,
    http: HttpClient,
}

impl NvSosSource {
    pub fn new(ctx: AdapterContext, urls: Vec<String>, http: HttpClient) -> Self {
        Self { ctx, urls, http }
    }
}

#[async_trait]
impl ResultsSource for NvSosSource {
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

/// `CarsonCity` → `Carson City`: a space before every inner capital.
pub fn county_from_attr(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len() + 2);
    for (i, ch) in stem.chars().enumerate() {
        if i > 0 && ch.is_ascii_uppercase() {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// `"493,052"` → 493052; empty means zero.
fn count(s: &str) -> Option<u64> {
    let digits: String = s.chars().filter(|c| !matches!(c, ',' | ' ')).collect();
    if digits.is_empty() {
        return Some(0);
    }
    digits.parse().ok()
}

/// One mapped race being read: county name → candidates.
struct RaceAcc {
    race_id: String,
    title: String,
    counties: BTreeMap<String, Vec<CandidateVotes>>,
}

/// Parse `USandNV.xml`. Races missing from the race map are skipped; a bad count fails the
/// payload; anything that is not a `USNVResults` document (an Incapsula challenge page) fails.
pub fn parse(body: &[u8], ctx: &AdapterContext, observed_at_ns: i64) -> Result<Parsed, ParseError> {
    let text = std::str::from_utf8(body).map_err(|e| ParseError(format!("utf-8: {e}")))?;
    let text = text.trim_start_matches('\u{feff}');
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut out = Parsed::default();
    let mut saw_root = false;
    let mut depth = 0usize;
    let mut race: Option<RaceAcc> = None;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| ParseError(format!("xml at {}: {e}", reader.error_position())))?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                depth += 1;
                match e.name().as_ref() {
                    "USNVResults" => saw_root = true,
                    "Race" => race = open_race(&e, ctx)?,
                    "Candidate" => add_candidate(&e, race.as_mut())?,
                    _ => {}
                }
            }
            Event::Empty(e) => match e.name().as_ref() {
                "Candidate" => add_candidate(&e, race.as_mut())?,
                "USNVResults" => saw_root = true,
                _ => {}
            },
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                if e.name().as_ref() == "Race"
                    && let Some(acc) = race.take()
                {
                    finish_race(acc, ctx, observed_at_ns, &mut out);
                }
            }
            _ => {}
        }
    }
    if !saw_root {
        return Err(ParseError("not a USNVResults document".into()));
    }
    if depth != 0 {
        return Err(ParseError("truncated USNVResults document".into()));
    }
    Ok(out)
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

fn open_race(e: &BytesStart<'_>, ctx: &AdapterContext) -> Result<Option<RaceAcc>, ParseError> {
    let title = attr(e, "RaceTitle")?.unwrap_or_default();
    Ok(ctx.races.get(&title).map(|race_id| RaceAcc {
        race_id: race_id.to_owned(),
        title: title.trim().to_owned(),
        counties: BTreeMap::new(),
    }))
}

fn add_candidate(e: &BytesStart<'_>, race: Option<&mut RaceAcc>) -> Result<(), ParseError> {
    let Some(race) = race else {
        return Ok(());
    };
    let name = attr(e, "CandidateName")?
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ParseError(format!("{}: candidate without a name", race.title)))?;
    let party = normalize_party(&attr(e, "Party")?.unwrap_or_default());
    for a in e.attributes() {
        let a = a.map_err(|err| ParseError(format!("{}: attribute: {err}", race.title)))?;
        let key = a.key.as_ref().to_owned();
        let Some(stem) = key.strip_suffix("Votes") else {
            continue;
        };
        if stem.is_empty() {
            continue; // the statewide total
        }
        let raw = a
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|err| ParseError(format!("attribute {key}: {err}")))?;
        let votes =
            count(&raw).ok_or_else(|| ParseError(format!("{}: bad {key}={raw:?} for {name}", race.title)))?;
        race.counties
            .entry(county_from_attr(stem))
            .or_default()
            .push(CandidateVotes {
                candidate: name.clone(),
                party: party.clone(),
                votes,
            });
    }
    Ok(())
}

fn finish_race(acc: RaceAcc, ctx: &AdapterContext, observed_at_ns: i64, out: &mut Parsed) {
    for (county, votes) in acc.counties {
        let Some(fips) = ctx.county_fips(&county) else {
            out.unresolved_counties += 1;
            continue;
        };
        out.results.push(ctx.result(
            &acc.race_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RaceMap;

    fn ctx() -> AdapterContext {
        AdapterContext {
            source_id: "nv_sos".into(),
            state: "NV".into(),
            races: RaceMap::from_pairs([("Governor", "2026-GOV-NV")]),
            tz: chrono_tz::Tz::America__Los_Angeles,
        }
    }

    #[test]
    fn camel_case_county_names() {
        assert_eq!(county_from_attr("CarsonCity"), "Carson City");
        assert_eq!(county_from_attr("WhitePine"), "White Pine");
        assert_eq!(county_from_attr("Clark"), "Clark");
    }

    #[test]
    fn reads_county_attributes_and_skips_top_two() {
        let xml = br#"<?xml version="1.0"?><USNVResults><UIPage Title="NVOther">
            <Race RaceID="9" RaceTitle="Governor" PrecinctsReported="1" TotalPrecincts="9">
              <Candidate CandidateName="A" Party="DEM" Votes="1,010" ClarkVotes="1,000" CarsonCityVotes="10" />
              <Candidate CandidateName="B" Party="REP" Votes="20" ClarkVotes="" CarsonCityVotes="20" />
            </Race>
            <RaceTopTwo RaceTitle="Governor"><CandidateTopTwo CandidateName="A" ClarkVotes="5" /></RaceTopTwo>
            <Race RaceTitle="Dog catcher"><Candidate CandidateName="C" ClarkVotes="1" /></Race>
            <Race RaceTitle="Governor"><Candidate CandidateName="D" AtlantisVotes="3" /></Race>
            </UIPage></USNVResults>"#;
        let p = parse(xml, &ctx(), 4).expect("parse");
        assert_eq!(p.unresolved_counties, 1);
        assert_eq!(p.results.len(), 2);
        let clark = p
            .results
            .iter()
            .find(|r| r.county_fips == "32003")
            .expect("Clark");
        assert_eq!(clark.votes.len(), 2);
        assert_eq!((clark.votes[0].votes, clark.votes[1].votes), (1_000, 0));
        assert_eq!(clark.vote_type, VoteType::Mixed as i32);
        let carson = p
            .results
            .iter()
            .find(|r| r.county_fips == "32510")
            .expect("Carson City");
        assert_eq!(carson.votes[1].party, "REP");
    }

    #[test]
    fn challenge_pages_and_bad_counts_fail() {
        let challenge = b"<!DOCTYPE html><html><head><title>Pardon Our Interruption</title></head></html>";
        assert!(parse(challenge, &ctx(), 0).is_err());
        let bad = br#"<USNVResults><Race RaceTitle="Governor"><Candidate CandidateName="A" ClarkVotes="many" /></Race></USNVResults>"#;
        assert!(parse(bad, &ctx(), 0).is_err());
        let truncated = br#"<USNVResults><Race RaceTitle="Governor">"#;
        assert!(parse(truncated, &ctx(), 0).is_err());
    }
}
