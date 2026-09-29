//! Normalization helpers shared by every adapter: vote-type tagging, party codes, tolerant
//! count parsing and the stable `batch_id` used for dedupe.

use chrono_tz::Tz;
use sha2::{Digest, Sha256};
use susq_contracts::v1::{CandidateVotes, CountyResult, VoteType};

/// Producer name stamped into every `CountyResultsBatch` header.
pub const PRODUCER: &str = "rust-live/enr";

/// Largest per-candidate or total count we accept for one county (LA County casts ~4M).
pub const MAX_COUNTY_VOTES: u64 = 20_000_000;

/// Map a source's vote-type label ("Absentee by Mail Votes", "VBM", "Election Day") to [`VoteType`].
///
/// Anything unrecognised, and the absence of a label, becomes [`VoteType::Mixed`]: the source does
/// not split by type, so downstream models must not assume a mail/E-day skew for it.
pub fn classify_vote_type(label: Option<&str>) -> VoteType {
    let Some(label) = label else {
        return VoteType::Mixed;
    };
    let l = label.to_ascii_lowercase();
    let has = |needle: &str| l.contains(needle);
    // "provi" also catches SC's real-world "Provionsal" typo.
    if has("provi") {
        VoteType::Provisional
    } else if has("mail") || has("absentee") || has("vbm") || has("postal") {
        VoteType::Mail
    } else if has("advance") || has("early") || has("one stop") {
        VoteType::EarlyInPerson
    } else if has("election day")
        || has("e-day")
        || has("eday")
        || has("polling place")
        || has("precinct")
        // SC "Failsafe": voters who moved, voting at the polls on Election Day.
        || has("failsafe")
    {
        VoteType::ElectionDay
    } else {
        VoteType::Mixed
    }
}

/// Canonical party code as used in `CandidateVotes.party`: DEM, REP, LIB, GRN, IND, or OTH.
pub fn normalize_party(raw: &str) -> String {
    let p = raw.trim().to_ascii_lowercase();
    let code = match p.as_str() {
        "" => "OTH",
        _ if p.starts_with("dem") || p == "d" || p == "dfl" => "DEM",
        _ if p.starts_with("rep") || p == "r" || p == "gop" => "REP",
        _ if p.starts_with("lib") || p == "l" => "LIB",
        _ if p.starts_with("gr") => "GRN",
        _ if p.starts_with("ind") || p == "i" || p == "npp" || p.starts_with("no party") || p == "np" => {
            "IND"
        }
        _ => "OTH",
    };
    code.to_owned()
}

/// Parse a vote count that sources encode as a JSON number or a string like `"1,234"`.
///
/// Returns `None` for negatives, fractions, non-finite numbers and anything unparseable; the
/// caller treats that as a malformed payload rather than guessing.
pub fn parse_count(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0 && f.fract() == 0.0 && *f < 9.0e15)
                .map(|f| f as u64)
        }),
        serde_json::Value::String(s) => {
            let cleaned: String = s.trim().chars().filter(|c| *c != ',' && *c != '_').collect();
            if cleaned.is_empty() {
                None
            } else {
                cleaned.parse::<u64>().ok()
            }
        }
        _ => None,
    }
}

/// Stable identity of one cumulative observation: sha256 over source, race, county, vote type,
/// totals, precinct counts and the candidate votes in sorted order (first 32 hex chars).
///
/// Two polls that return the same numbers produce the same id, which is how the pipeline dedupes.
/// Timestamps are deliberately excluded: a source re-stamping unchanged numbers is not new data.
pub fn batch_id(r: &CountyResult) -> String {
    let mut cands: Vec<&CandidateVotes> = r.votes.iter().collect();
    cands.sort_by(|a, b| (&a.candidate, &a.party).cmp(&(&b.candidate, &b.party)));
    let mut h = Sha256::new();
    for part in [&r.source, &r.race_id, &r.county_fips] {
        h.update(part.as_bytes());
        h.update([0x1f]);
    }
    h.update(r.vote_type.to_le_bytes());
    h.update(r.total_votes.to_le_bytes());
    h.update(r.precincts_reporting.to_le_bytes());
    h.update(r.precincts_total.to_le_bytes());
    for c in cands {
        h.update(c.candidate.as_bytes());
        h.update([0x1f]);
        h.update(c.party.as_bytes());
        h.update([0x1f]);
        h.update(c.votes.to_le_bytes());
    }
    let digest = hex::encode(h.finalize());
    digest[..32].to_owned()
}

/// Key used to line up the same candidate across polls of one source.
pub fn candidate_key(c: &CandidateVotes) -> String {
    format!("{}|{}", c.candidate.trim().to_ascii_lowercase(), c.party)
}

/// Key used to line up candidates across *different* sources, which spell names differently:
/// the two major parties by code, everyone else pooled as `OTH`.
pub fn cross_source_key(c: &CandidateVotes) -> &'static str {
    match c.party.as_str() {
        "DEM" => "DEM",
        "REP" => "REP",
        _ => "OTH",
    }
}

/// Current UTC time in nanoseconds since the epoch.
pub fn now_ns() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX)
}

/// Parse a source timestamp (RFC 3339, or a zone-less local time such as Clarity's
/// `M/D/YYYY h:mm:ss AM/PM`, interpreted in `tz`) into ns since the epoch. Returns 0 when
/// unparseable, as the contract specifies for "no source timestamp".
pub fn parse_source_ts(s: &str, tz: Tz) -> i64 {
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_nanos_opt().unwrap_or(0);
    }
    ["%m/%d/%Y %I:%M:%S %p", "%m/%d/%Y %H:%M:%S", "%Y-%m-%d %H:%M:%S"]
        .iter()
        .find_map(|fmt| chrono::NaiveDateTime::parse_from_str(s, fmt).ok())
        .map_or(0, |naive| local_to_ns(naive, tz))
}

/// A wall-clock time in `tz` → ns since the epoch (the earlier instant on a DST fold; 0 in a gap).
pub fn local_to_ns(naive: chrono::NaiveDateTime, tz: Tz) -> i64 {
    use chrono::TimeZone;
    tz.from_local_datetime(&naive)
        .earliest()
        .and_then(|dt| dt.timestamp_nanos_opt())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vote_type_labels_from_real_sources() {
        assert_eq!(classify_vote_type(Some("Absentee by Mail Votes")), VoteType::Mail);
        assert_eq!(
            classify_vote_type(Some("Advance Voting Votes")),
            VoteType::EarlyInPerson
        );
        assert_eq!(
            classify_vote_type(Some("Election Day Votes")),
            VoteType::ElectionDay
        );
        assert_eq!(
            classify_vote_type(Some("Provisional Votes")),
            VoteType::Provisional
        );
        assert_eq!(classify_vote_type(Some("Provionsal")), VoteType::Provisional);
        assert_eq!(
            classify_vote_type(Some("Failsafe Provisional")),
            VoteType::Provisional
        );
        assert_eq!(classify_vote_type(Some("Failsafe")), VoteType::ElectionDay);
        assert_eq!(classify_vote_type(Some("ONE STOP")), VoteType::EarlyInPerson);
        assert_eq!(classify_vote_type(Some("Absentee By Mail")), VoteType::Mail);
        assert_eq!(classify_vote_type(Some("VBM")), VoteType::Mail);
        assert_eq!(classify_vote_type(Some("Total")), VoteType::Mixed);
        assert_eq!(classify_vote_type(None), VoteType::Mixed);
    }

    /// Shared with `py/tests/replay/test_records.py`: Python replays and the Rust poller must
    /// agree on batch ids.
    #[test]
    fn batch_id_cross_language_vector() {
        assert_eq!(batch_id(&sample()), "432a72f243d38bf29eb50d61d2a4739d");
    }

    #[test]
    fn parties_normalize() {
        assert_eq!(normalize_party("Dem"), "DEM");
        assert_eq!(normalize_party("Democratic"), "DEM");
        assert_eq!(normalize_party("REP"), "REP");
        assert_eq!(normalize_party("No Party Preference"), "IND");
        assert_eq!(normalize_party("Green"), "GRN");
        assert_eq!(normalize_party("Peace and Freedom"), "OTH");
        assert_eq!(normalize_party(""), "OTH");
    }

    #[test]
    fn counts_parse_strictly() {
        assert_eq!(parse_count(&json!(12)), Some(12));
        assert_eq!(parse_count(&json!("7,097,947")), Some(7_097_947));
        assert_eq!(parse_count(&json!(12.0)), Some(12));
        assert_eq!(parse_count(&json!(12.5)), None);
        assert_eq!(parse_count(&json!(-3)), None);
        assert_eq!(parse_count(&json!("")), None);
        assert_eq!(parse_count(&json!("n/a")), None);
        assert_eq!(parse_count(&json!(null)), None);
    }

    fn sample() -> CountyResult {
        CountyResult {
            race_id: "2026-SEN-GA".into(),
            state: "GA".into(),
            county_fips: "13121".into(),
            source: "clarity:GA".into(),
            vote_type: VoteType::Mail as i32,
            votes: vec![
                CandidateVotes {
                    candidate: "A".into(),
                    party: "DEM".into(),
                    votes: 10,
                },
                CandidateVotes {
                    candidate: "B".into(),
                    party: "REP".into(),
                    votes: 7,
                },
            ],
            total_votes: 17,
            ..Default::default()
        }
    }

    #[test]
    fn batch_id_ignores_order_and_timestamps_but_not_numbers() {
        let a = sample();
        let mut b = sample();
        b.votes.reverse();
        b.observed_at_ns = 99;
        b.source_ts_ns = 42;
        assert_eq!(batch_id(&a), batch_id(&b));
        let mut c = sample();
        c.votes[0].votes = 11;
        assert_ne!(batch_id(&a), batch_id(&c));
        let mut d = sample();
        d.vote_type = VoteType::ElectionDay as i32;
        assert_ne!(batch_id(&a), batch_id(&d));
    }

    #[test]
    fn source_timestamps() {
        assert_eq!(
            parse_source_ts("2026-11-04T01:02:03Z", Tz::UTC),
            1_793_754_123_000_000_000
        );
        // Clarity style; Nov 3 2026 is after DST ends, so New York is UTC-5.
        assert_eq!(
            parse_source_ts("11/3/2026 8:02:03 PM", Tz::America__New_York),
            1_793_754_123_000_000_000
        );
        // Same wall clock in October is EDT (UTC-4): one hour earlier in UTC.
        assert_eq!(
            parse_source_ts("10/27/2026 8:02:03 PM", Tz::America__New_York),
            1_793_754_123_000_000_000 - 7 * 86_400_000_000_000 - 3_600_000_000_000
        );
        assert_eq!(parse_source_ts("garbage", Tz::UTC), 0);
    }
}
