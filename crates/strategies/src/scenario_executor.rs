//! Scenario executor: work positions toward the Python-selected scenario book in tranches.
//!
//! For each `TargetPosition`, `gap = target_qty − position` (signed, +YES / −NO). Each decision sends
//! one tranche of `clamp(ceil(|gap| × tranche_frac), min_tranche_qty, |gap|)` shares, always as a buy:
//! YES when the gap is positive, NO when it is negative (positions are one signed number per exchange,
//! so buying NO reduces a YES holding and vice versa). A tranche that would flip the position through
//! zero stops at flat; the other side is opened on a later decision.
//!
//! Guards:
//! * a tranche that increases `|position|` is skipped while the target file is stale or invalid
//!   (`targets_increase_allowed == false`); reductions still proceed;
//! * a `locked` leg is never reduced;
//! * only verified markets, only fresh books, and nothing on an exchange with a live scenario intent.
//!
//! Price cap. `max_entry_price` is a YES-space price (proto: "YES-space price cap for buys (NO buys use
//! 1 − this as floor)"). Buying YES requires a price ≤ `max_entry_price`, so the limit is
//! `min(best ask, floor(max_entry_price))`: take the ask when it is within the cap, otherwise rest at the
//! cap. Buying NO requires the YES-equivalent price `1 − p_no` to be ≥ `max_entry_price`, i.e. the NO
//! limit is `min(best NO ask, floor(1 − max_entry_price))`. A cap that yields no valid price (for
//! example the proto default 0.0 on a YES buy) means no order.

use susq_contracts::v1::TargetPosition;
use susq_core::{Action, ExchangeId, OrderBundle, Pips, Side, config::ScenarioCfg};

use crate::{
    Strategy,
    draft::Draft,
    snapshot::{Decision, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "scenario";

/// The scenario executor.
#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioExecutor {
    pub cfg: ScenarioCfg,
}

/// One step toward a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tranche {
    /// Side to buy.
    pub side: Side,
    /// Shares in this tranche.
    pub qty: i64,
    /// Whether the step reduces `|position|` (otherwise it increases it).
    pub reduces: bool,
}

impl ScenarioExecutor {
    /// Scenario executor with its config.
    pub fn new(cfg: ScenarioCfg) -> Self {
        Self { cfg }
    }

    /// Next tranche from `position` toward `target`, or `None` when already there.
    pub fn tranche(&self, position: i64, target: i64) -> Option<Tranche> {
        let gap = target.checked_sub(position)?;
        if gap == 0 {
            return None;
        }
        let abs_gap = gap.unsigned_abs().min(i64::MAX as u64) as i64;
        let frac = (abs_gap as f64 * self.cfg.tranche_frac.max(0.0)).ceil();
        let raw = if frac >= i64::MAX as f64 {
            i64::MAX
        } else {
            frac as i64
        };
        let mut qty = raw.max(self.cfg.min_tranche_qty).min(abs_gap).max(1);
        let reduces = position != 0 && gap.signum() != position.signum();
        if reduces {
            qty = qty.min(position.abs());
        }
        let side = if gap > 0 { Side::Yes } else { Side::No };
        Some(Tranche { side, qty, reduces })
    }

    fn work(&self, snap: &Snapshot<'_>, tp: &TargetPosition, d: &mut Decision) {
        let ex = ExchangeId::new(tp.exchange_id.clone());
        if !snap.markets.is_verified(&ex) || snap.has_live_intent(NAME, &ex) {
            return;
        }
        let Some(t) = self.tranche(snap.position(&ex), tp.target_qty) else {
            return;
        };
        if t.reduces && tp.locked {
            d.note(format!("scenario {ex}: locked leg, not reducing"));
            return;
        }
        if !t.reduces && !snap.targets_increase_allowed {
            d.note(format!(
                "scenario {ex}: targets stale/invalid: not increasing exposure"
            ));
            return;
        }
        let Some(book) = snap.fresh_book(&ex) else {
            d.note(format!("scenario {ex}: no fresh book"));
            return;
        };
        let cap = match t.side {
            Side::Yes => Pips::floor_from_prob(tp.max_entry_price),
            Side::No => Pips::floor_from_prob(1.0 - tp.max_entry_price),
        };
        let Some(cap) = cap else {
            d.note(format!(
                "scenario {ex}: no valid {} price under max_entry_price {}",
                t.side, tp.max_entry_price
            ));
            return;
        };
        let price = book.touch(t.side, Action::Buy).map_or(cap, |ask| ask.min(cap));
        let draft = Draft {
            exchange: ex.clone(),
            side: t.side,
            action: Action::Buy,
            qty: t.qty,
            price,
            fair_yes: None,
        };
        if let Some(spec) = draft.into_spec(snap, NAME, ex.as_str(), self.cfg.order_ttl_s, false) {
            d.note(format!(
                "scenario {ex}: buy {} x{} @ {price} toward {}",
                t.side, t.qty, tp.target_qty
            ));
            d.bundles.push(OrderBundle::single(spec));
        }
    }
}

impl Strategy for ScenarioExecutor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn decide(&self, snap: &Snapshot<'_>) -> Decision {
        let mut d = Decision::default();
        if !self.cfg.enabled {
            return d;
        }
        let Some(targets) = snap.targets else {
            return d;
        };
        for tp in &targets.positions {
            self.work(snap, tp, &mut d);
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{World, intent};

    fn exec() -> ScenarioExecutor {
        ScenarioExecutor::new(ScenarioCfg::default())
    }

    fn world() -> World {
        let mut w = World::new(100_000.0);
        w.rule("1", "m1", "", "", true);
        w.book("1", &[(3000, 500)], &[(3200, 500)]);
        w
    }

    fn only_leg(d: &Decision) -> (Side, u32, i64) {
        assert_eq!(d.bundles.len(), 1, "{:?}", d.notes);
        let l = &d.bundles[0].legs[0];
        (l.side, l.price.get(), l.quantity.get())
    }

    #[test]
    fn tranche_sizes() {
        let e = exec();
        assert_eq!(
            e.tranche(0, 100),
            Some(Tranche {
                side: Side::Yes,
                qty: 25,
                reduces: false
            })
        );
        assert_eq!(
            e.tranche(0, -3),
            Some(Tranche {
                side: Side::No,
                qty: 1,
                reduces: false
            })
        );
        assert_eq!(e.tranche(90, 100).map(|t| t.qty), Some(3));
        assert_eq!(
            e.tranche(100, 0),
            Some(Tranche {
                side: Side::No,
                qty: 25,
                reduces: true
            })
        );
        // Through zero: stop at flat first.
        assert_eq!(
            e.tranche(10, -200),
            Some(Tranche {
                side: Side::No,
                qty: 10,
                reduces: true
            })
        );
        assert_eq!(e.tranche(7, 7), None);
        let big_min = ScenarioExecutor::new(ScenarioCfg {
            min_tranche_qty: 40,
            ..ScenarioCfg::default()
        });
        assert_eq!(big_min.tranche(0, 100).map(|t| t.qty), Some(40));
        assert_eq!(
            big_min.tranche(0, 30).map(|t| t.qty),
            Some(30),
            "never beyond the gap"
        );
    }

    #[test]
    fn buys_yes_at_the_ask_within_the_cap_else_rests_at_the_cap() {
        let mut w = world();
        w.target("1", 100, 0.35, false);
        assert_eq!(only_leg(&exec().decide(&w.snap())), (Side::Yes, 3200, 25));
        let mut w = world();
        w.target("1", 100, 0.25, false);
        assert_eq!(only_leg(&exec().decide(&w.snap())), (Side::Yes, 2500, 25));
    }

    #[test]
    fn no_buys_are_capped_at_one_minus_max_entry() {
        // Target -100 with max_entry 0.72: the YES-equivalent must be ≥ 0.72, so NO ≤ 0.28.
        // NO ask is 1 − 0.30 = 0.70, far above the cap, so the order rests at 0.28.
        let mut w = world();
        w.target("1", -100, 0.72, false);
        assert_eq!(only_leg(&exec().decide(&w.snap())), (Side::No, 2800, 25));
        // With max_entry 0.25 the cap is 0.75 and the 0.70 NO ask is taken.
        let mut w = world();
        w.target("1", -100, 0.25, false);
        assert_eq!(only_leg(&exec().decide(&w.snap())), (Side::No, 7000, 25));
    }

    #[test]
    fn locked_legs_are_never_reduced() {
        let mut w = world();
        w.position("1", 100);
        w.target("1", 0, 0.9, true);
        let d = exec().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("locked")));
        // Adding to a locked leg is fine.
        let mut w = world();
        w.position("1", 100);
        w.target("1", 200, 0.9, true);
        assert_eq!(only_leg(&exec().decide(&w.snap())).0, Side::Yes);
    }

    #[test]
    fn stale_targets_block_increases_but_allow_decreases() {
        let mut w = world();
        w.targets_increase_allowed = false;
        w.target("1", 100, 0.9, false);
        let d = exec().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("not increasing exposure")));

        let mut w = world();
        w.targets_increase_allowed = false;
        w.position("1", -80);
        w.target("1", 0, 0.9, false);
        assert_eq!(only_leg(&exec().decide(&w.snap())), (Side::Yes, 3200, 20));
    }

    #[test]
    fn skips_without_targets_unverified_worked_or_stale_books() {
        let w = world();
        assert!(exec().decide(&w.snap()).is_empty());

        let mut w = world();
        w.rule("1", "m1", "", "", false);
        w.target("1", 100, 0.9, false);
        assert!(exec().decide(&w.snap()).is_empty());

        let mut w = world();
        w.target("1", 100, 0.9, false);
        w.intents
            .push(intent("scenario:1:1", NAME, "1", Side::Yes, Action::Buy, 3200));
        assert!(exec().decide(&w.snap()).is_empty());

        let mut w = world();
        w.stale_book("1", &[(3000, 500)], &[(3200, 500)]);
        w.target("1", 100, 0.9, false);
        let d = exec().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("no fresh book")));
    }
}
