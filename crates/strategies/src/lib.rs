//! Constraints racer, sniper, market maker, scenario executor, election-night executor.
//!
//! Every strategy is a pure decision function: it reads one [`Snapshot`] of the world (books, fair
//! values, positions, our live intents, targets, needle posteriors, relationship violations) and
//! returns a [`Decision`] (order bundles to send, intents to cancel, notes for the journal). No I/O,
//! no clocks, no hidden state, so every decision is reproducible and unit-testable.
//!
//! Shared rules every strategy follows:
//! * intent ids are deterministic: `"{strategy}:{key}:{now_s}"`;
//! * nothing new is sent for something the same strategy is already working;
//! * only fresh books are traded (no fresh book → no trade, noted);
//! * quantities are whole shares and at least 1.

pub mod constraints_racer;
mod draft;
pub mod en_executor;
pub mod market_maker;
pub mod scenario_executor;
pub mod snapshot;
pub mod sniper;
#[cfg(test)]
mod testkit;

pub use constraints_racer::ConstraintsRacer;
pub use en_executor::EnExecutor;
pub use market_maker::MarketMaker;
pub use scenario_executor::ScenarioExecutor;
pub use snapshot::{CancelRequest, Decision, FairQuote, IntentView, NeedleView, Snapshot};
pub use sniper::Sniper;

/// A trading strategy: a pure function from a snapshot to a decision.
pub trait Strategy {
    /// Short name; also the prefix of every intent id the strategy emits.
    fn name(&self) -> &'static str;
    /// Decide what to send and cancel given the current world.
    fn decide(&self, snap: &Snapshot<'_>) -> Decision;
}

/// Run every strategy on the same snapshot, in order.
pub fn run_all(strategies: &[&dyn Strategy], snap: &Snapshot<'_>) -> Vec<(&'static str, Decision)> {
    strategies.iter().map(|s| (s.name(), s.decide(snap))).collect()
}

#[cfg(test)]
mod tests {
    use proptest::{
        collection::vec,
        option,
        prelude::{any, prop_assert, prop_assert_eq, proptest},
    };
    use susq_core::{
        Pips,
        config::{EnCfg, MmCfg, RacerCfg, ScenarioCfg, SniperCfg},
    };
    use susq_platform::{NodeRole, RelationshipType};

    use super::*;
    use crate::testkit::{World, arb_book, node, rel, violation};

    const EXCHANGES: [&str; 4] = ["1", "2", "3", "4"];
    const RACE: &str = "2026-SEN-PA";

    proptest! {
        /// Whatever any strategy emits on a random world is a sendable order: valid on-tick price,
        /// at least one share, an intent id prefixed by the strategy name, and needle provenance only
        /// from the election-night executor.
        #[test]
        fn every_emitted_order_is_well_formed(
            books in vec(arb_book(), 4),
            fairs in vec((0.01f64..0.99, option::of(0.01f64..0.99)), 4),
            positions in vec(-600i64..600, 4),
            targets in vec((-300i64..300, 0.0f64..1.0, any::<bool>()), 4),
            p_dem in 0.0f64..1.0,
            n_batches in 0u32..20,
            equity in 1.0f64..1e6,
            util in 0.0f64..1.5,
            stale_targets in any::<bool>(),
        ) {
            let mut w = World::new(equity);
            for (i, ex) in EXCHANGES.iter().enumerate() {
                let party = if i % 2 == 0 { "DEM" } else { "REP" };
                w.rule(ex, "m1", RACE, party, true);
                w.rule_with(ex, |r| r.clusters = vec!["national".into()]);
                w.book(ex, &books[i].0, &books[i].1);
                w.fair(ex, fairs[i].0, fairs[i].1);
                w.position(ex, positions[i]);
                w.target(ex, targets[i].0, targets[i].1, targets[i].2);
            }
            w.targets_increase_allowed = !stale_targets;
            w.cluster_utilization.insert("national".into(), util);
            w.needle(true, &[(RACE, p_dem, 1.0 - p_dem, n_batches)]);
            w.relationship(rel(
                "me",
                RelationshipType::MutuallyExclusive,
                true,
                ["1", "2", "3"].iter().map(|e| node(e, NodeRole::Member)).collect(),
            ));
            w.relationship(rel(
                "imp",
                RelationshipType::Implication,
                false,
                vec![node("3", NodeRole::Antecedent), node("4", NodeRole::Consequent)],
            ));
            w.violations.push(violation("me", RelationshipType::MutuallyExclusive));
            w.violations.push(violation("imp", RelationshipType::Implication));

            let racer = ConstraintsRacer::new(RacerCfg::default());
            let sniper = Sniper::new(SniperCfg::default());
            let mm = MarketMaker::new(MmCfg { enabled: true, ..MmCfg::default() });
            let scenario = ScenarioExecutor::new(ScenarioCfg::default());
            let en = EnExecutor::new(EnCfg { enabled: true, ..EnCfg::default() });
            let all: [&dyn Strategy; 5] = [&racer, &sniper, &mm, &scenario, &en];
            let snap = w.snap();
            for (name, d) in run_all(&all, &snap) {
                let prefix = format!("{name}:");
                let suffix = format!(":{}", snap.now_s());
                for b in &d.bundles {
                    prop_assert!(!b.legs.is_empty() && b.legs.len() <= constraints_racer::MAX_LEGS);
                    for l in &b.legs {
                        prop_assert!(Pips::new(l.price.get()).is_ok());
                        prop_assert!(l.quantity.get() >= 1);
                        prop_assert!(l.intent_id.starts_with(&prefix), "{}", l.intent_id);
                        prop_assert!(l.intent_id.ends_with(&suffix), "{}", l.intent_id);
                        prop_assert_eq!(l.strategy.as_str(), name);
                        prop_assert_eq!(l.needle_derived, name == en_executor::NAME);
                        prop_assert!(!l.ttl.is_zero());
                    }
                }
            }
        }
    }

    #[test]
    fn run_all_keeps_strategy_order_and_names() {
        let w = World::new(1_000.0);
        let racer = ConstraintsRacer::new(RacerCfg::default());
        let sniper = Sniper::new(SniperCfg::default());
        let out = run_all(&[&racer, &sniper], &w.snap());
        let names: Vec<&str> = out.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["racer", "sniper"]);
        assert!(out.iter().all(|(_, d)| d.is_empty()));
    }
}
