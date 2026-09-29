//! Sniper: take resting orders that sit at least `min_edge_pts` through a fresh fair value.
//!
//! * Fair must be fresh (`now - observed_at <= fair.stale_after_s`); on a stale fair the sniper stops
//!   and cancels its resting orders on that exchange.
//! * Only verified market rules are traded.
//! * Mapping-error gate: a book mid more than `fair.mapping_error_pts` from fair almost always means the
//!   fair source is mapped to the wrong side or contract, so the exchange is blocked and flagged.
//! * Buy YES when best ask ≤ fair − edge (limit = floor(fair − edge)); buy NO when best bid ≥ fair + edge
//!   (NO limit = floor((1 − fair) − edge)). Depth is walked up to the limit.
//! * Size: a wrong fill may cost at most `max_mistake_frac` of equity, i.e.
//!   `qty ≤ floor(max_mistake_frac × equity / limit)` in the bought side's terms.

use susq_core::{Action, ExchangeId, OrderBundle, Pips, Side, config::SniperCfg};

use crate::{
    Strategy,
    draft::{Draft, shares_for_budget},
    snapshot::{Decision, FairQuote, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "sniper";

/// The sniper.
#[derive(Debug, Clone, PartialEq)]
pub struct Sniper {
    pub cfg: SniperCfg,
}

impl Sniper {
    /// Sniper with its config.
    pub fn new(cfg: SniperCfg) -> Self {
        Self { cfg }
    }

    fn snipe(&self, snap: &Snapshot<'_>, ex: &ExchangeId, fair: &FairQuote, d: &mut Decision) {
        if !snap.is_fair_fresh(fair) {
            let mut cancelled = false;
            for i in snap.live_intents(NAME).filter(|i| &i.exchange_id == ex) {
                d.cancel(&i.intent_id, "stale fair");
                cancelled = true;
            }
            if cancelled {
                d.note(format!("sniper {ex}: stale fair, cancelling resting orders"));
            }
            return;
        }
        if !snap.markets.is_verified(ex) || snap.has_live_intent(NAME, ex) {
            return;
        }
        let Some(book) = snap.fresh_book(ex) else {
            d.note(format!("sniper {ex}: no fresh book"));
            return;
        };
        let reference = book.mid().or_else(|| {
            book.best_bid()
                .or_else(|| book.best_ask())
                .map(|l| l.price.prob())
        });
        let Some(reference) = reference else {
            return;
        };
        if (reference - fair.p_yes).abs() > snap.fair_cfg.mapping_error_pts / 100.0 {
            d.note(format!(
                "sniper {ex}: mapping error suspected (book {reference:.3} vs fair {:.3}), blocked",
                fair.p_yes
            ));
            return;
        }
        let edge = self.cfg.min_edge_pts / 100.0;
        for side in [Side::Yes, Side::No] {
            let side_fair = match side {
                Side::Yes => fair.p_yes,
                Side::No => 1.0 - fair.p_yes,
            };
            let Some(limit) = Pips::floor_from_prob(side_fair - edge) else {
                continue;
            };
            if !book.touch(side, Action::Buy).is_some_and(|t| t <= limit) {
                continue;
            }
            let cap = shares_for_budget(self.cfg.max_mistake_frac * snap.equity, limit);
            let sweep = book.sweep(side, Action::Buy, cap, limit);
            let draft = Draft {
                exchange: ex.clone(),
                side,
                action: Action::Buy,
                qty: sweep.qty,
                price: limit,
                fair_yes: Some(fair.p_yes),
            };
            let key = format!("{ex}:{side}");
            match draft.into_spec(snap, NAME, &key, self.cfg.order_ttl_s, false) {
                Some(spec) => {
                    d.note(format!(
                        "sniper {ex}: buy {side} x{} @ {limit} (fair {:.3})",
                        spec.quantity, fair.p_yes
                    ));
                    d.bundles.push(OrderBundle::single(spec));
                }
                None => d.note(format!("sniper {ex}: {side} edge but size < 1 (cap {cap})")),
            }
        }
    }
}

impl Strategy for Sniper {
    fn name(&self) -> &'static str {
        NAME
    }

    fn decide(&self, snap: &Snapshot<'_>) -> Decision {
        let mut d = Decision::default();
        if !self.cfg.enabled {
            return d;
        }
        for (ex, fair) in snap.fair_sorted() {
            self.snipe(snap, ex, fair, &mut d);
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{World, intent};

    fn sniper() -> Sniper {
        Sniper::new(SniperCfg::default())
    }

    fn world() -> World {
        let mut w = World::new(100_000.0);
        w.rule("1", "m1", "", "", true);
        w
    }

    #[test]
    fn buys_yes_through_fair_walking_depth_to_the_limit() {
        let mut w = world();
        w.fair("1", 0.60, None);
        // Edge 3.5 pts → YES limit floor(0.565) = 0.565. Asks at 0.55 and 0.56 are taken, 0.57 is not.
        w.book("1", &[(5000, 10)], &[(5500, 20), (5600, 30), (5700, 100)]);
        let d = sniper().decide(&w.snap());
        assert_eq!(d.bundles.len(), 1, "{:?}", d.notes);
        let o = &d.bundles[0].legs[0];
        assert_eq!((o.side, o.action, o.price.get()), (Side::Yes, Action::Buy, 5650));
        assert_eq!(o.quantity.get(), 50);
        assert_eq!(o.fair_yes, Some(0.60));
        assert_eq!(o.ttl.as_secs(), 30);
        assert!(o.intent_id.starts_with("sniper:1:yes:"));
    }

    #[test]
    fn buys_no_when_the_bid_is_through_fair() {
        let mut w = world();
        w.fair("1", 0.40, None);
        // Bid 0.45 ≥ 0.40 + 0.035 → buy NO at ≤ floor(0.60 − 0.035) = 0.565 (NO ask 0.55).
        w.book("1", &[(4500, 8), (4300, 50)], &[(4800, 10)]);
        let d = sniper().decide(&w.snap());
        let o = &d.bundles[0].legs[0];
        assert_eq!((o.side, o.price.get(), o.quantity.get()), (Side::No, 5650, 8));
    }

    #[test]
    fn sizes_so_a_mistake_costs_under_the_cap() {
        let mut w = world();
        w.equity = 10_000.0; // 1% = 100 SUSQies at a 0.565 limit → 176 shares.
        w.fair("1", 0.60, None);
        w.book("1", &[(5000, 10)], &[(5500, 1_000)]);
        let d = sniper().decide(&w.snap());
        let o = &d.bundles[0].legs[0];
        assert_eq!(o.quantity.get(), 176);
        assert!(o.max_notional() < 0.01 * w.equity);
    }

    #[test]
    fn stale_fair_blocks_and_cancels_resting_orders() {
        let mut w = world();
        w.fair_at("1", 0.60, None, w.now_ns - 61 * 1_000_000_000);
        w.book("1", &[(5000, 10)], &[(5500, 20)]);
        w.intents
            .push(intent("sniper:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5650));
        w.intents
            .push(intent("mm:1:bid:1", "mm", "1", Side::Yes, Action::Buy, 5000));
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 1);
        assert_eq!(
            (d.cancels[0].intent_id.as_str(), d.cancels[0].reason.as_str()),
            ("sniper:1:yes:1", "stale fair")
        );
    }

    #[test]
    fn mapping_error_gate_blocks_far_books() {
        let mut w = world();
        w.fair("1", 0.80, None); // an inverted mapping would show the book near 0.20
        w.book("1", &[(1900, 10)], &[(2100, 10)]);
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("mapping error suspected")));
    }

    #[test]
    fn skips_unverified_worked_and_edge_free_exchanges() {
        let mut w = world();
        w.rule("2", "m2", "", "", false);
        w.fair("1", 0.60, None);
        w.fair("2", 0.60, None);
        w.book("1", &[(5000, 10)], &[(5500, 20)]);
        w.book("2", &[(5000, 10)], &[(5500, 20)]);
        w.intents
            .push(intent("sniper:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5650));
        assert!(sniper().decide(&w.snap()).bundles.is_empty());

        let mut w = world();
        w.fair("1", 0.60, None);
        w.book("1", &[(5800, 10)], &[(6000, 20)]);
        assert!(
            sniper().decide(&w.snap()).bundles.is_empty(),
            "ask 0.60 is not through fair"
        );
    }

    #[test]
    fn no_fresh_book_means_no_trade() {
        let mut w = world();
        w.fair("1", 0.60, None);
        w.stale_book("1", &[(5000, 10)], &[(5500, 20)]);
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("no fresh book")));
    }
}
