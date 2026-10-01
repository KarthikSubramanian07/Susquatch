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
//! Pacing. The book is bought gradually under the platform's 30 order writes per minute: nothing
//! before `start_utc`, at least `min_interval_s` between tranches on one exchange, and no tranche
//! once the notional (shares × price) emitted in the trailing hour exceeds
//! `max_hourly_notional_frac` of equity. Filled tranches leave the live-intent view, so the
//! executor remembers what it emitted (time, exchange, notional) for the trailing hour; a restart
//! forgets that history, which only ever errs toward sending a tranche earlier.
//!
//! Price cap. `max_entry_price` is a YES-space price (proto: "YES-space price cap for buys (NO buys use
//! 1 − this as floor)"). Buying YES requires a price ≤ `max_entry_price`, so the limit is
//! `min(best ask, floor(max_entry_price))`: take the ask when it is within the cap, otherwise rest at the
//! cap. Buying NO requires the YES-equivalent price `1 − p_no` to be ≥ `max_entry_price`, i.e. the NO
//! limit is `min(best NO ask, floor(1 − max_entry_price))`. A cap that yields no valid price (for
//! example the proto default 0.0 on a YES buy) means no order.

use std::sync::Mutex;

use susq_contracts::v1::TargetPosition;
use susq_core::{Action, ExchangeId, OrderBundle, Pips, Side, clock::to_ns, config::ScenarioCfg};

use crate::{
    Strategy,
    draft::Draft,
    snapshot::{Decision, NS_PER_S, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "scenario";
/// Window of the hourly notional cap.
const HOUR_NS: i64 = 3600 * NS_PER_S;

/// A tranche this executor emitted.
#[derive(Debug, Clone, PartialEq)]
struct Sent {
    at_ns: i64,
    exchange: ExchangeId,
    /// Shares × price, in the order's side terms.
    notional: f64,
}

/// The scenario executor.
#[derive(Debug)]
pub struct ScenarioExecutor {
    pub cfg: ScenarioCfg,
    /// Tranches emitted in the trailing hour (pruned on every decision).
    sent: Mutex<Vec<Sent>>,
}

/// Why no tranche goes out this decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paused {
    /// Before `start_utc`.
    NotStarted,
    /// The trailing hour's notional already exceeds the cap.
    HourlyCap,
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
        Self {
            cfg,
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Drop emitted tranches older than an hour and return the trailing-hour notional.
    fn prune_sent(&self, now_ns: i64) -> f64 {
        let Ok(mut sent) = self.sent.lock() else {
            return f64::INFINITY;
        };
        sent.retain(|s| now_ns.saturating_sub(s.at_ns) < HOUR_NS);
        sent.iter().map(|s| s.notional).sum()
    }

    /// Whether anything may go out at all this decision.
    fn paused(&self, snap: &Snapshot<'_>, hourly_notional: f64) -> Option<Paused> {
        if self.cfg.start_utc.is_some_and(|t| snap.now_ns < to_ns(t)) {
            return Some(Paused::NotStarted);
        }
        if self.cfg.max_hourly_notional_frac < 1.0
            && hourly_notional > self.cfg.max_hourly_notional_frac * snap.equity.max(0.0)
        {
            return Some(Paused::HourlyCap);
        }
        None
    }

    /// When this executor last emitted a tranche on `ex` (a still-live intent there already
    /// blocks a new one before this is consulted).
    fn last_tranche_ns(&self, ex: &ExchangeId) -> Option<i64> {
        self.sent
            .lock()
            .ok()
            .and_then(|s| s.iter().filter(|s| &s.exchange == ex).map(|s| s.at_ns).max())
    }

    /// Whether `ex` had a tranche less than `min_interval_s` ago.
    fn too_soon(&self, now_ns: i64, ex: &ExchangeId) -> bool {
        let min_ns = i64::try_from(self.cfg.min_interval_s)
            .unwrap_or(i64::MAX)
            .saturating_mul(NS_PER_S);
        min_ns > 0
            && self
                .last_tranche_ns(ex)
                .is_some_and(|t| now_ns.saturating_sub(t) < min_ns)
    }

    fn record(&self, now_ns: i64, exchange: ExchangeId, notional: f64) {
        if let Ok(mut sent) = self.sent.lock() {
            sent.push(Sent {
                at_ns: now_ns,
                exchange,
                notional,
            });
        }
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

    /// Work one target. Returns the notional emitted (0 when nothing was).
    fn work(&self, snap: &Snapshot<'_>, tp: &TargetPosition, d: &mut Decision) -> f64 {
        let ex = ExchangeId::new(tp.exchange_id.clone());
        if !snap.markets.is_verified(&ex) || snap.has_live_intent(NAME, &ex) {
            return 0.0;
        }
        let Some(t) = self.tranche(snap.position(&ex), tp.target_qty) else {
            return 0.0;
        };
        if self.too_soon(snap.now_ns, &ex) {
            d.note(format!(
                "scenario {ex}: last tranche under {} s ago, waiting",
                self.cfg.min_interval_s
            ));
            return 0.0;
        }
        if t.reduces && tp.locked {
            d.note(format!("scenario {ex}: locked leg, not reducing"));
            return 0.0;
        }
        if !t.reduces && !snap.targets_increase_allowed {
            d.note(format!(
                "scenario {ex}: targets stale/invalid: not increasing exposure"
            ));
            return 0.0;
        }
        let Some(book) = snap.fresh_book(&ex) else {
            d.note(format!("scenario {ex}: no fresh book"));
            return 0.0;
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
            return 0.0;
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
        let Some(spec) = draft.into_spec(snap, NAME, ex.as_str(), self.cfg.order_ttl_s, false) else {
            return 0.0;
        };
        d.note(format!(
            "scenario {ex}: buy {} x{} @ {price} toward {}",
            t.side, t.qty, tp.target_qty
        ));
        let notional = spec.max_notional();
        d.bundles.push(OrderBundle::single(spec));
        self.record(snap.now_ns, ex, notional);
        notional
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
        let mut hourly = self.prune_sent(snap.now_ns);
        for tp in &targets.positions {
            match self.paused(snap, hourly) {
                Some(Paused::NotStarted) => {
                    d.note("scenario: before start_utc, not started");
                    break;
                }
                Some(Paused::HourlyCap) => {
                    d.note(format!(
                        "scenario: trailing-hour notional {hourly:.2} over {} of equity, pausing",
                        self.cfg.max_hourly_notional_frac
                    ));
                    break;
                }
                None => hourly += self.work(snap, tp, &mut d),
            }
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{NOW_S, World, intent};

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

    fn paced(edit: impl FnOnce(&mut ScenarioCfg)) -> ScenarioExecutor {
        let mut cfg = ScenarioCfg::default();
        edit(&mut cfg);
        ScenarioExecutor::new(cfg)
    }

    /// Re-stamp the books at the world's (advanced) clock so they are fresh again.
    fn refresh_books(w: &mut World, exchanges: &[&str]) {
        for ex in exchanges {
            w.book(ex, &[(3000, 500)], &[(3200, 500)]);
        }
    }

    fn paced_from(secs: i64) -> ScenarioExecutor {
        paced(|c| c.start_utc = Some(susq_core::clock::from_ns(secs * NS_PER_S)))
    }

    #[test]
    fn nothing_goes_out_before_start_utc() {
        let mut w = world();
        w.target("1", 100, 0.9, false);
        let e = paced_from(NOW_S + 3600);
        let d = e.decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("not started")), "{:?}", d.notes);
        // At or after the start it trades; `None` means immediately.
        let e = paced_from(NOW_S);
        assert_eq!(only_leg(&e.decide(&w.snap())), (Side::Yes, 3200, 25));
        assert_eq!(only_leg(&paced(|_| {}).decide(&w.snap())).2, 25);
    }

    #[test]
    fn min_interval_spaces_tranches_on_one_exchange() {
        let e = paced(|c| c.min_interval_s = 900);
        let mut w = world();
        w.rule("2", "m2", "", "", true);
        w.book("2", &[(3000, 500)], &[(3200, 500)]);
        w.target("1", 100, 0.9, false);
        w.target("2", 100, 0.9, false);
        assert_eq!(
            e.decide(&w.snap()).bundles.len(),
            2,
            "both exchanges get a first tranche"
        );
        // The tranches filled (no live intents, positions moved): the same exchanges must wait.
        w.position("1", 25);
        w.position("2", 25);
        w.now_ns += 899 * NS_PER_S;
        refresh_books(&mut w, &["1", "2"]);
        let d = e.decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", d.notes);
        assert_eq!(d.notes.iter().filter(|n| n.contains("waiting")).count(), 2);
        // A new exchange is not held back by the others.
        w.rule("3", "m3", "", "", true);
        w.book("3", &[(3000, 500)], &[(3200, 500)]);
        w.target("3", 100, 0.9, false);
        let d = e.decide(&w.snap());
        assert_eq!(d.bundles.len(), 1);
        assert_eq!(d.bundles[0].legs[0].exchange_id, ExchangeId::new("3"));
        // Once the interval has passed the next tranche goes out (3 was just worked).
        w.now_ns += NS_PER_S;
        refresh_books(&mut w, &["1", "2", "3"]);
        let d = e.decide(&w.snap());
        assert_eq!(d.bundles.len(), 2, "{:?}", d.notes);
        // The default (0) never waits.
        let e = paced(|_| {});
        let mut w = world();
        w.target("1", 100, 0.9, false);
        assert_eq!(e.decide(&w.snap()).bundles.len(), 1);
        w.position("1", 25);
        assert_eq!(e.decide(&w.snap()).bundles.len(), 1);
    }

    #[test]
    fn hourly_notional_cap_pauses_and_releases_after_an_hour() {
        // Equity 100 000 × 0.005 = 500 of notional per trailing hour; one tranche is
        // 2 500 shares × 0.32 = 800, so the first goes out and the next waits for the window.
        let e = paced(|c| c.max_hourly_notional_frac = 0.005);
        let mut w = world();
        w.rule("2", "m2", "", "", true);
        w.book("2", &[(3000, 500)], &[(3200, 500)]);
        w.target("1", 10_000, 0.9, false);
        w.target("2", 10_000, 0.9, false);
        let d = e.decide(&w.snap());
        assert_eq!(
            d.bundles.len(),
            1,
            "the cap holds within one decision: {:?}",
            d.notes
        );
        assert_eq!(d.bundles[0].legs[0].max_notional(), 800.0);
        assert!(d.notes.iter().any(|n| n.contains("pausing")), "{:?}", d.notes);
        w.position("1", 2_500);
        w.now_ns += 3599 * NS_PER_S;
        refresh_books(&mut w, &["1", "2"]);
        let d = e.decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", d.notes);
        w.now_ns += NS_PER_S;
        refresh_books(&mut w, &["1", "2"]);
        let d = e.decide(&w.snap());
        assert_eq!(
            d.bundles.len(),
            1,
            "the window slid past the first tranche: {:?}",
            d.notes
        );
        // Unknown equity (0) with a cap sends nothing; the default 1.0 means no cap at all.
        let mut w = world();
        w.equity = 0.0;
        w.target("1", 10_000, 0.9, false);
        let e = paced(|c| c.max_hourly_notional_frac = 0.5);
        e.decide(&w.snap());
        w.position("1", 2_500);
        assert!(e.decide(&w.snap()).bundles.is_empty());
        let e = paced(|_| {});
        e.decide(&w.snap());
        w.position("1", 4_375);
        assert_eq!(e.decide(&w.snap()).bundles.len(), 1);
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
