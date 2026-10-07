//! Everything between "an adapter parsed a payload" and "a result goes on the bus":
//!
//! 1. **Validation**: reject malformed results (never panic) and count them.
//! 2. **Tagging**: an unspecified vote type becomes `MIXED`.
//! 3. **Dedupe**: an unchanged cumulative observation (same `batch_id`) is dropped.
//! 4. **Monotonicity**: counts are cumulative, so a decrease is held as a *correction* and only
//!    applied once the source repeats it for `confirm_corrections_after` consecutive polls.
//! 5. **Cross-source consistency**: when two sources report the same county, they must agree
//!    within tolerance; a disagreement marks the race *disputed* until it resolves.
//!
//! A series is `(source, race, county, vote_type)`. The pipeline is synchronous and owned by a
//! single task, so it needs no locking.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use susq_contracts::v1::{CountyResult, VoteType};

use crate::config::ConsistencyConfig;
use crate::model::{MAX_COUNTY_VOTES, batch_id, candidate_key, cross_source_key};

/// Pipeline tuning.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub consistency: ConsistencyConfig,
    pub confirm_corrections_after: u32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            consistency: ConsistencyConfig::default(),
            confirm_corrections_after: 2,
        }
    }
}

/// Why a result was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    MissingField(&'static str),
    BadState(String),
    BadFips(String),
    NoCandidates,
    DuplicateCandidate(String),
    VotesExceedTotal { sum: u64, total: u64 },
    PrecinctsExceedTotal { reporting: u32, total: u32 },
    Implausible(u64),
    UnknownVoteType(i32),
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField(n) => write!(f, "missing {n}"),
            Self::BadState(s) => write!(f, "bad state {s:?}"),
            Self::BadFips(s) => write!(f, "bad county fips or reporting unit {s:?}"),
            Self::NoCandidates => write!(f, "no candidates"),
            Self::DuplicateCandidate(c) => write!(f, "candidate listed twice: {c}"),
            Self::VotesExceedTotal { sum, total } => write!(f, "candidate votes {sum} exceed total {total}"),
            Self::PrecinctsExceedTotal { reporting, total } => {
                write!(f, "precincts reporting {reporting} exceed total {total}")
            }
            Self::Implausible(n) => write!(f, "implausible count {n}"),
            Self::UnknownVoteType(v) => write!(f, "unknown vote type {v}"),
        }
    }
}

/// Something the pipeline did that operators and executors should know about.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineEvent {
    Rejected {
        source: String,
        reason: RejectReason,
    },
    /// A count went down; held until the source confirms it.
    CorrectionHeld {
        source: String,
        race_id: String,
        county_fips: String,
        detail: String,
    },
    /// A confirmed decrease was applied as the new cumulative baseline.
    CorrectionApplied {
        source: String,
        race_id: String,
        county_fips: String,
    },
    DisputeOpened {
        race_id: String,
        county_fips: String,
        sources: (String, String),
        detail: String,
    },
    DisputeCleared {
        race_id: String,
        county_fips: String,
        sources: (String, String),
    },
}

/// Output of one [`Pipeline::ingest`] call.
#[derive(Debug, Default)]
pub struct IngestOutcome {
    /// Results to publish, with `batch_id` set.
    pub accepted: Vec<CountyResult>,
    pub events: Vec<PipelineEvent>,
}

/// Running counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PipelineStats {
    pub accepted: u64,
    pub duplicates: u64,
    pub rejected: u64,
    pub corrections_held: u64,
    pub corrections_applied: u64,
    pub disputes_opened: u64,
}

type SeriesKey = (String, String, String, i32);
type CountyKey = (String, String);
type DisputeKey = (String, String, String); // (county, source a, source b), a < b

/// The dedupe / monotonicity / consistency state machine.
#[derive(Debug, Default)]
pub struct Pipeline {
    cfg: PipelineConfig,
    last: HashMap<SeriesKey, CountyResult>,
    pending: HashMap<SeriesKey, (String, u32)>,
    sources_by_county: HashMap<CountyKey, BTreeSet<String>>,
    disputes: BTreeMap<String, BTreeMap<DisputeKey, String>>,
    stats: PipelineStats,
}

impl Pipeline {
    pub fn new(cfg: PipelineConfig) -> Self {
        Self {
            cfg,
            ..Self::default()
        }
    }

    pub fn stats(&self) -> &PipelineStats {
        &self.stats
    }

    /// Races where two sources currently disagree beyond tolerance. Executors must not act on them.
    pub fn disputed_races(&self) -> BTreeSet<String> {
        self.disputes
            .iter()
            .filter(|(_, d)| !d.is_empty())
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Current dispute details per race (county, sources, reason).
    pub fn dispute_details(&self) -> BTreeMap<String, Vec<String>> {
        self.disputes
            .iter()
            .filter(|(_, d)| !d.is_empty())
            .map(|(race, d)| {
                let lines = d
                    .iter()
                    .map(|((county, a, b), why)| format!("{county} {a} vs {b}: {why}"))
                    .collect();
                (race.clone(), lines)
            })
            .collect()
    }

    /// Last accepted cumulative result for a series, if any.
    pub fn last_accepted(
        &self,
        source: &str,
        race_id: &str,
        county_fips: &str,
        vote_type: VoteType,
    ) -> Option<&CountyResult> {
        self.last.get(&(
            source.to_owned(),
            race_id.to_owned(),
            county_fips.to_owned(),
            vote_type as i32,
        ))
    }

    /// Run a poll's worth of results through every stage.
    pub fn ingest(&mut self, results: Vec<CountyResult>) -> IngestOutcome {
        let mut out = IngestOutcome::default();
        for raw in results {
            let source = raw.source.clone();
            let mut r = match validate(raw) {
                Ok(r) => r,
                Err(reason) => {
                    self.stats.rejected += 1;
                    out.events.push(PipelineEvent::Rejected { source, reason });
                    continue;
                }
            };
            r.batch_id = batch_id(&r);
            let key: SeriesKey = (
                r.source.clone(),
                r.race_id.clone(),
                r.county_fips.clone(),
                r.vote_type,
            );

            if let Some(prev) = self.last.get(&key) {
                if prev.batch_id == r.batch_id {
                    self.stats.duplicates += 1;
                    self.pending.remove(&key);
                    continue;
                }
                if let Some(detail) = decrease(prev, &r) {
                    let entry = self
                        .pending
                        .entry(key.clone())
                        .or_insert_with(|| (r.batch_id.clone(), 0));
                    if entry.0 != r.batch_id {
                        *entry = (r.batch_id.clone(), 0);
                    }
                    entry.1 += 1;
                    if entry.1 < self.cfg.confirm_corrections_after.max(1) {
                        self.stats.corrections_held += 1;
                        out.events.push(PipelineEvent::CorrectionHeld {
                            source: r.source.clone(),
                            race_id: r.race_id.clone(),
                            county_fips: r.county_fips.clone(),
                            detail,
                        });
                        continue;
                    }
                    self.stats.corrections_applied += 1;
                    out.events.push(PipelineEvent::CorrectionApplied {
                        source: r.source.clone(),
                        race_id: r.race_id.clone(),
                        county_fips: r.county_fips.clone(),
                    });
                    // Tell downstream (needle) this decrease is a confirmed correction, not a regression.
                    r.is_correction = true;
                }
            }

            self.pending.remove(&key);
            self.stats.accepted += 1;
            let county_key = (r.race_id.clone(), r.county_fips.clone());
            self.sources_by_county
                .entry(county_key.clone())
                .or_default()
                .insert(r.source.clone());
            self.last.insert(key, r.clone());
            out.accepted.push(r);
            self.check_consistency(&county_key, &mut out.events);
        }
        out
    }

    /// Compare every pair of sources that report this county.
    fn check_consistency(&mut self, county: &CountyKey, events: &mut Vec<PipelineEvent>) {
        let Some(sources) = self.sources_by_county.get(county) else {
            return;
        };
        if sources.len() < 2 {
            return;
        }
        let sources: Vec<String> = sources.iter().cloned().collect();
        let aggs: Vec<Agg> = sources.iter().map(|s| self.aggregate(s, county)).collect();
        let (race, fips) = county;
        for i in 0..sources.len() {
            for j in (i + 1)..sources.len() {
                let dkey = (fips.clone(), sources[i].clone(), sources[j].clone());
                let verdict = compare(&aggs[i], &aggs[j], &self.cfg.consistency);
                let race_disputes = self.disputes.entry(race.clone()).or_default();
                match verdict {
                    Some(detail) => {
                        if race_disputes.insert(dkey, detail.clone()).is_none() {
                            self.stats.disputes_opened += 1;
                            events.push(PipelineEvent::DisputeOpened {
                                race_id: race.clone(),
                                county_fips: fips.clone(),
                                sources: (sources[i].clone(), sources[j].clone()),
                                detail,
                            });
                        }
                    }
                    None => {
                        if race_disputes.remove(&dkey).is_some() {
                            events.push(PipelineEvent::DisputeCleared {
                                race_id: race.clone(),
                                county_fips: fips.clone(),
                                sources: (sources[i].clone(), sources[j].clone()),
                            });
                        }
                    }
                }
            }
        }
    }

    /// Sum a source's latest results for one county over all vote types.
    fn aggregate(&self, source: &str, (race, fips): &CountyKey) -> Agg {
        let mut agg = Agg::default();
        for vt in ALL_VOTE_TYPES {
            if let Some(r) = self
                .last
                .get(&(source.to_owned(), race.clone(), fips.clone(), vt as i32))
            {
                agg.total += r.total_votes;
                for c in &r.votes {
                    *agg.by_key.entry(cross_source_key(c)).or_default() += c.votes;
                }
            }
        }
        agg
    }
}

const ALL_VOTE_TYPES: [VoteType; 5] = [
    VoteType::Mail,
    VoteType::EarlyInPerson,
    VoteType::ElectionDay,
    VoteType::Provisional,
    VoteType::Mixed,
];

#[derive(Debug, Default)]
struct Agg {
    total: u64,
    by_key: BTreeMap<&'static str, u64>,
}

impl Agg {
    fn get(&self, k: &str) -> u64 {
        self.by_key.get(k).copied().unwrap_or(0)
    }
}

/// `Some(reason)` if two sources' views of one county are inconsistent.
///
/// Sources update at different times, so a lower total is fine *as long as it is a plausible
/// earlier snapshot*: no party may have more votes in the lagging source than in the leading one.
/// When totals match (within tolerance), the two-party shares must match too.
fn compare(a: &Agg, b: &Agg, cfg: &ConsistencyConfig) -> Option<String> {
    let (lo, hi) = if a.total <= b.total { (a, b) } else { (b, a) };
    if hi.total == 0 {
        return None;
    }
    let tol = |x: u64| (cfg.abs_tol_votes as f64).max(cfg.total_tol_frac * x as f64);
    for k in ["DEM", "REP", "OTH"] {
        let (l, h) = (lo.get(k), hi.get(k));
        if l as f64 > h as f64 + tol(h) {
            return Some(format!("{k}: lagging source has {l} > leading source {h}"));
        }
    }
    let same_count = (hi.total - lo.total) as f64 <= tol(hi.total);
    let two_party = |x: &Agg| {
        let (d, r) = (x.get("DEM") as f64, x.get("REP") as f64);
        (d + r > 0.0).then(|| d / (d + r))
    };
    if same_count && let (Some(sl), Some(sh)) = (two_party(lo), two_party(hi)) {
        let diff_pts = (sl - sh).abs() * 100.0;
        if diff_pts > cfg.share_tol_pts {
            return Some(format!(
                "same count but DEM two-party share differs by {diff_pts:.2} pts"
            ));
        }
    }
    None
}

/// `Some(description)` if `new` has fewer votes than `prev` anywhere.
fn decrease(prev: &CountyResult, new: &CountyResult) -> Option<String> {
    if new.total_votes < prev.total_votes {
        return Some(format!("total {} -> {}", prev.total_votes, new.total_votes));
    }
    let now: HashMap<String, u64> = new.votes.iter().map(|c| (candidate_key(c), c.votes)).collect();
    for c in &prev.votes {
        let n = now.get(&candidate_key(c)).copied().unwrap_or(0);
        if n < c.votes {
            return Some(format!("{} {} -> {}", c.candidate, c.votes, n));
        }
    }
    None
}

/// Structural validation plus vote-type tagging. Fills `total_votes` from the candidate sum when
/// the source gave none.
fn validate(mut r: CountyResult) -> Result<CountyResult, RejectReason> {
    if r.source.trim().is_empty() {
        return Err(RejectReason::MissingField("source"));
    }
    if r.race_id.trim().is_empty() {
        return Err(RejectReason::MissingField("race_id"));
    }
    if r.state.len() != 2 || !r.state.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(RejectReason::BadState(r.state));
    }
    // A county FIPS, or a House district reported as one unit (`CA-HD22`, see `crate::units`).
    if !crate::units::is_unit_key(&r.county_fips, &r.state) {
        return Err(RejectReason::BadFips(r.county_fips));
    }
    match VoteType::try_from(r.vote_type) {
        Ok(VoteType::Unspecified) => r.vote_type = VoteType::Mixed as i32,
        Ok(_) => {}
        Err(_) => return Err(RejectReason::UnknownVoteType(r.vote_type)),
    }
    if r.votes.is_empty() {
        return Err(RejectReason::NoCandidates);
    }
    let mut seen = BTreeSet::new();
    let mut sum: u64 = 0;
    for c in &r.votes {
        if c.candidate.trim().is_empty() {
            return Err(RejectReason::MissingField("candidate"));
        }
        if !seen.insert(candidate_key(c)) {
            return Err(RejectReason::DuplicateCandidate(c.candidate.clone()));
        }
        if c.votes > MAX_COUNTY_VOTES {
            return Err(RejectReason::Implausible(c.votes));
        }
        sum += c.votes;
    }
    if r.total_votes > MAX_COUNTY_VOTES {
        return Err(RejectReason::Implausible(r.total_votes));
    }
    if r.total_votes == 0 {
        r.total_votes = sum;
    } else if sum > r.total_votes {
        return Err(RejectReason::VotesExceedTotal {
            sum,
            total: r.total_votes,
        });
    }
    if r.precincts_total > 0 && r.precincts_reporting > r.precincts_total {
        return Err(RejectReason::PrecinctsExceedTotal {
            reporting: r.precincts_reporting,
            total: r.precincts_total,
        });
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_contracts::v1::CandidateVotes;

    fn result(source: &str, fips: &str, vt: VoteType, dem: u64, rep: u64) -> CountyResult {
        CountyResult {
            race_id: "2026-SEN-GA".into(),
            state: "GA".into(),
            county_fips: fips.into(),
            source: source.into(),
            vote_type: vt as i32,
            votes: vec![
                CandidateVotes {
                    candidate: "Ossoff".into(),
                    party: "DEM".into(),
                    votes: dem,
                },
                CandidateVotes {
                    candidate: "Rival".into(),
                    party: "REP".into(),
                    votes: rep,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn accepts_then_dedupes() {
        let mut p = Pipeline::default();
        let out = p.ingest(vec![result("a", "13121", VoteType::Mail, 100, 50)]);
        assert_eq!(out.accepted.len(), 1);
        assert_eq!(out.accepted[0].total_votes, 150);
        assert_eq!(out.accepted[0].batch_id.len(), 32);
        let out = p.ingest(vec![result("a", "13121", VoteType::Mail, 100, 50)]);
        assert!(out.accepted.is_empty());
        assert_eq!(p.stats().duplicates, 1);
    }

    #[test]
    fn unspecified_vote_type_is_tagged_mixed() {
        let mut p = Pipeline::default();
        let out = p.ingest(vec![result("a", "13121", VoteType::Unspecified, 1, 1)]);
        assert_eq!(out.accepted[0].vote_type, VoteType::Mixed as i32);
    }

    #[test]
    fn rejects_malformed() {
        let mut p = Pipeline::default();
        let mut bad_total = result("a", "13121", VoteType::Mail, 100, 50);
        bad_total.total_votes = 10;
        let mut bad_fips = result("a", "1312", VoteType::Mail, 1, 1);
        bad_fips.county_fips = "1312".into();
        let mut dup = result("a", "13121", VoteType::Mail, 1, 1);
        dup.votes.push(dup.votes[0].clone());
        let mut prec = result("a", "13121", VoteType::Mail, 1, 1);
        prec.precincts_reporting = 5;
        prec.precincts_total = 4;
        let mut vt = result("a", "13121", VoteType::Mail, 1, 1);
        vt.vote_type = 42;
        let mut empty = result("a", "13121", VoteType::Mail, 1, 1);
        empty.votes.clear();
        let out = p.ingest(vec![bad_total, bad_fips, dup, prec, vt, empty]);
        assert!(out.accepted.is_empty());
        assert_eq!(p.stats().rejected, 6);
    }

    #[test]
    fn house_district_units_are_accepted_for_their_own_state_only() {
        let mut p = Pipeline::default();
        let mut ca22 = result("a", "CA-HD22", VoteType::Mixed, 10, 9);
        ca22.state = "CA".into();
        ca22.race_id = "2026-HOUSE-CA-22".into();
        let mut wrong_state = ca22.clone();
        wrong_state.state = "NV".into();
        let mut malformed = ca22.clone();
        malformed.county_fips = "CA-22".into();
        let out = p.ingest(vec![ca22, wrong_state, malformed]);
        assert_eq!(out.accepted.len(), 1);
        assert_eq!(out.accepted[0].county_fips, "CA-HD22");
        assert_eq!(p.stats().rejected, 2);
    }

    #[test]
    fn decrease_is_held_then_applied_when_confirmed() {
        let mut p = Pipeline::default();
        p.ingest(vec![result("a", "13121", VoteType::Mail, 100, 50)]);
        let out = p.ingest(vec![result("a", "13121", VoteType::Mail, 90, 50)]);
        assert!(out.accepted.is_empty());
        assert!(matches!(out.events[0], PipelineEvent::CorrectionHeld { .. }));
        let out = p.ingest(vec![result("a", "13121", VoteType::Mail, 90, 50)]);
        assert_eq!(out.accepted.len(), 1);
        assert!(matches!(out.events[0], PipelineEvent::CorrectionApplied { .. }));
        assert_eq!(p.stats().corrections_applied, 1);
        assert!(
            out.accepted[0].is_correction,
            "confirmed decrease must be flagged for the needle"
        );
        let out = p.ingest(vec![result("a", "13121", VoteType::Mail, 95, 50)]); // normal growth again
        assert!(!out.accepted[0].is_correction);
    }

    #[test]
    fn transient_decrease_is_never_applied() {
        let mut p = Pipeline::default();
        p.ingest(vec![result("a", "13121", VoteType::Mail, 100, 50)]);
        p.ingest(vec![result("a", "13121", VoteType::Mail, 90, 50)]); // glitch
        let out = p.ingest(vec![result("a", "13121", VoteType::Mail, 100, 50)]); // back to normal
        assert!(out.accepted.is_empty());
        let out = p.ingest(vec![result("a", "13121", VoteType::Mail, 90, 50)]); // glitch again: count restarts
        assert!(out.accepted.is_empty());
        assert_eq!(p.stats().corrections_applied, 0);
    }

    #[test]
    fn lagging_source_is_consistent() {
        let mut p = Pipeline::default();
        p.ingest(vec![result("a", "13121", VoteType::Mixed, 10_000, 8_000)]);
        let out = p.ingest(vec![result("b", "13121", VoteType::Mixed, 6_000, 5_000)]);
        assert!(p.disputed_races().is_empty(), "{:?}", out.events);
    }

    #[test]
    fn disagreement_opens_and_clears_dispute() {
        let mut p = Pipeline::default();
        p.ingest(vec![result("a", "13121", VoteType::Mixed, 10_000, 8_000)]);
        let out = p.ingest(vec![result("b", "13121", VoteType::Mixed, 8_000, 10_000)]);
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, PipelineEvent::DisputeOpened { .. }))
        );
        assert!(p.disputed_races().contains("2026-SEN-GA"));
        // Source b fixes its data: REP goes down, so it is a correction and needs confirming.
        p.ingest(vec![result("b", "13121", VoteType::Mixed, 10_000, 8_000)]);
        assert!(
            p.disputed_races().contains("2026-SEN-GA"),
            "held correction must not clear it"
        );
        let out = p.ingest(vec![result("b", "13121", VoteType::Mixed, 10_000, 8_000)]);
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, PipelineEvent::DisputeCleared { .. }))
        );
        assert!(p.disputed_races().is_empty());
    }

    #[test]
    fn split_and_unsplit_sources_compare_on_county_totals() {
        let mut p = Pipeline::default();
        p.ingest(vec![
            result("clarity", "13121", VoteType::Mail, 4_000, 2_000),
            result("clarity", "13121", VoteType::ElectionDay, 6_000, 6_000),
        ]);
        p.ingest(vec![result("feed", "13121", VoteType::Mixed, 10_000, 8_000)]);
        assert!(p.disputed_races().is_empty());
    }

    #[test]
    fn small_counties_do_not_flap() {
        let mut p = Pipeline::default();
        p.ingest(vec![result("a", "13001", VoteType::Mixed, 30, 10)]);
        p.ingest(vec![result("b", "13001", VoteType::Mixed, 40, 5)]);
        // Totals within the absolute floor but shares 14 pts apart: a real disagreement.
        assert!(!p.disputed_races().is_empty());
        // Same shares, totals within the floor: consistent, even though one doubled.
        let mut q = Pipeline::default();
        q.ingest(vec![result("a", "13001", VoteType::Mixed, 30, 10)]);
        q.ingest(vec![result("b", "13001", VoteType::Mixed, 60, 20)]);
        assert!(q.disputed_races().is_empty());
    }
}
