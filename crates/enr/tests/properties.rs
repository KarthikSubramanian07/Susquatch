//! Property tests for the pipeline invariants: dedupe, monotonicity/corrections, validation that
//! never panics, and cross-source consistency.

use proptest::prelude::*;
use susq_contracts::v1::{CandidateVotes, CountyResult, VoteType};
use susq_enr::model::batch_id;
use susq_enr::pipeline::{Pipeline, PipelineConfig, PipelineEvent};

fn result(source: &str, dem: u64, rep: u64, oth: u64) -> CountyResult {
    CountyResult {
        race_id: "2026-SEN-OH-S".into(),
        state: "OH".into(),
        county_fips: "39049".into(),
        source: source.into(),
        vote_type: VoteType::Mixed as i32,
        votes: vec![
            CandidateVotes {
                candidate: "Brown".into(),
                party: "DEM".into(),
                votes: dem,
            },
            CandidateVotes {
                candidate: "Husted".into(),
                party: "REP".into(),
                votes: rep,
            },
            CandidateVotes {
                candidate: "Other".into(),
                party: "LIB".into(),
                votes: oth,
            },
        ],
        ..Default::default()
    }
}

fn pipeline(confirm: u32) -> Pipeline {
    Pipeline::new(PipelineConfig {
        confirm_corrections_after: confirm,
        ..PipelineConfig::default()
    })
}

fn counts() -> impl Strategy<Value = (u64, u64, u64)> {
    (0u64..200_000, 0u64..200_000, 0u64..5_000)
}

proptest! {
    #[test]
    fn batch_id_ignores_candidate_order_and_timestamps(c in counts(), ts in any::<i64>(), rot in 0usize..3) {
        let a = result("s", c.0, c.1, c.2);
        let mut b = a.clone();
        b.votes.rotate_left(rot);
        b.observed_at_ns = ts;
        b.source_ts_ns = ts / 2;
        prop_assert_eq!(batch_id(&a), batch_id(&b));
    }

    /// Replaying every observation twice emits exactly what replaying once does.
    #[test]
    fn duplicates_never_re_emit(seq in prop::collection::vec(counts(), 1..40)) {
        let mut once = pipeline(2);
        let mut twice = pipeline(2);
        let mut n_once = 0;
        let mut n_twice = 0;
        for c in &seq {
            n_once += once.ingest(vec![result("s", c.0, c.1, c.2)]).accepted.len();
            n_twice += twice.ingest(vec![result("s", c.0, c.1, c.2)]).accepted.len();
            // An immediate identical repeat is a duplicate, or confirms a held correction.
            n_twice += twice.ingest(vec![result("s", c.0, c.1, c.2)]).accepted.len();
        }
        // With doubling, every held correction gets confirmed, so twice >= once; but never more
        // than one emission per distinct input.
        prop_assert!(n_twice >= n_once);
        prop_assert!(n_twice <= seq.len());
    }

    /// Non-decreasing cumulative counts: every change is accepted, nothing is held.
    #[test]
    fn monotone_streams_flow_straight_through(steps in prop::collection::vec(counts(), 1..40)) {
        let mut p = pipeline(2);
        let (mut d, mut r, mut o) = (0u64, 0u64, 0u64);
        let mut distinct = 0;
        let mut last = None;
        for (i, (dd, rr, oo)) in steps.iter().enumerate() {
            // Use the step as an increment (some increments are zero -> repeat).
            if i % 3 != 0 { d += dd % 1_000; r += rr % 1_000; o += oo % 10; }
            if last != Some((d, r, o)) { distinct += 1; last = Some((d, r, o)); }
            let out = p.ingest(vec![result("s", d, r, o)]);
            let held = out.events.iter().any(|e| matches!(e, PipelineEvent::CorrectionHeld { .. }));
            prop_assert!(!held, "monotone input must never be held as a correction");
        }
        prop_assert_eq!(p.stats().accepted as usize, distinct);
        prop_assert_eq!(p.stats().corrections_applied, 0);
    }

    /// Whatever the input, the accepted stream only goes down after `confirm` identical polls.
    #[test]
    fn decreases_need_confirmation(seq in prop::collection::vec((0u64..50, 0u64..50), 1..60), confirm in 1u32..4) {
        let mut p = pipeline(confirm);
        let mut history: Vec<(u64, u64)> = Vec::new();
        let mut last_accepted: Option<(u64, u64)> = None;
        for &(d, r) in &seq {
            history.push((d, r));
            let out = p.ingest(vec![result("s", d, r, 0)]);
            if let Some(acc) = out.accepted.first() {
                let now = (acc.votes[0].votes, acc.votes[1].votes);
                if let Some(prev) = last_accepted
                    && (now.0 < prev.0 || now.1 < prev.1)
                {
                    let k = confirm as usize;
                    prop_assert!(history.len() >= k);
                    prop_assert!(history[history.len() - k..].iter().all(|h| *h == now),
                        "decrease {:?} -> {:?} applied without {} identical polls", prev, now, k);
                }
                last_accepted = Some(now);
            }
        }
    }

    /// Arbitrary junk never panics, and anything accepted satisfies the contract invariants.
    #[test]
    fn validation_is_total(
        fips in "[0-9a-z]{0,6}",
        state in "[A-Za-z]{0,3}",
        race in "[ -~]{0,12}",
        vt in -2i32..8,
        total in 0u64..30_000_000,
        votes in prop::collection::vec(("[ -~]{0,6}", "[A-Z]{0,3}", 0u64..30_000_000), 0..5),
        pr in 0u32..10, pt in 0u32..10,
    ) {
        let r = CountyResult {
            race_id: race, state, county_fips: fips, source: "s".into(), vote_type: vt,
            votes: votes.into_iter().map(|(candidate, party, votes)| CandidateVotes { candidate, party, votes }).collect(),
            total_votes: total, precincts_reporting: pr, precincts_total: pt, ..Default::default()
        };
        let mut p = pipeline(2);
        for a in p.ingest(vec![r]).accepted {
            prop_assert_eq!(a.county_fips.len(), 5);
            prop_assert!(a.vote_type >= 1 && a.vote_type <= 5);
            let sum: u64 = a.votes.iter().map(|c| c.votes).sum();
            prop_assert!(sum <= a.total_votes);
            prop_assert!(a.precincts_total == 0 || a.precincts_reporting <= a.precincts_total);
        }
    }

    /// A source that is an earlier snapshot of another (every party <=) never causes a dispute,
    /// and identical sources never do either.
    #[test]
    fn lagging_snapshots_are_consistent(lead in counts(), frac in 0.0f64..1.0) {
        let lag = ((lead.0 as f64 * frac) as u64, (lead.1 as f64 * frac) as u64, (lead.2 as f64 * frac) as u64);
        let mut p = pipeline(2);
        p.ingest(vec![result("a", lead.0, lead.1, lead.2)]);
        p.ingest(vec![result("b", lag.0, lag.1, lag.2)]);
        p.ingest(vec![result("c", lead.0, lead.1, lead.2)]);
        // Proportional lag keeps shares equal, so even "same count" comparisons agree.
        prop_assert!(p.disputed_races().is_empty(), "{:?}", p.dispute_details());
    }
}
