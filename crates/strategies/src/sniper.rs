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
//!
//! # Unwind (EXP-003)
//!
//! From `unwind_from_utc` the sniper buys nothing and sells its book to free cash for the scenario
//! book and the election-night budget. Positions are account-level (and the bridge bought most of
//! them before the cutover), so the *unwind set* is a rule, identical in the bridge
//! (`bridge_py/susq_bridge/strategies/sniper.py`, `plan_unwind`). A held side is in the set when
//!
//! * the market is verified, its fair is fresh, its book is fresh and passes the mapping-error gate;
//! * the held side's fair is at least 0.90 (YES held and fair ≥ 0.90, or NO held and fair ≤ 0.10);
//! * the exchange is not a locked leg of the target file;
//! * no other strategy has a live intent on the exchange, and the sniper has none there either;
//! * (rust-live only, election night) the needle has no posterior for the market's race: those
//!   races belong to the election-night executor. The bridge has no needle, so before the night the
//!   rule is the same in both traders.
//!
//! Exchanges are taken in id order. Each sell is covered (at most the holding, so the position never
//! flips) and priced at `max(held-side bid, ceil_tick(side fair − unwind_max_haircut_pts))`, never
//! below that floor. Its size is the smallest of the holding, the per-order rule (proceeds at most
//! `max_mistake_frac` of equity), the cash still missing to `unwind_target_cash_frac` of equity, and
//! what is left of `unwind_max_hourly_frac` of equity in proceeds this trailing hour. The unwind
//! stops once cash reaches the target and waits while cash is unknown.
//! `tests/fixtures/sniper_unwind.json` holds both traders to the same sells.

use std::sync::Mutex;

use susq_book::L2Book;
use susq_core::{
    Action, ExchangeId, OrderBundle, Pips, Side,
    clock::to_ns,
    config::SniperCfg,
    price::{PIPS_PER_UNIT, TICK},
};

use crate::{
    Strategy,
    draft::{Draft, shares_for_budget},
    snapshot::{Decision, FairQuote, NS_PER_S, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "sniper";
/// Journal method of one unwind sell.
pub const UNWIND_EVENT: &str = "sniper_unwind";
/// A held side is in the unwind set only when its fair is at least 0.90 (pips).
pub const UNWIND_MIN_SIDE_FAIR_PIPS: i64 = 9_000;
/// Window of the hourly unwind cap.
const HOUR_NS: i64 = 3600 * NS_PER_S;
const PIPS: f64 = PIPS_PER_UNIT as f64;

/// The sniper.
#[derive(Debug)]
pub struct Sniper {
    pub cfg: SniperCfg,
    /// Unwind sells emitted in the trailing hour: (when, proceeds in SUSQies). A restart forgets
    /// them, which only lets the first hour after it sell a little more.
    sent: Mutex<Vec<(i64, f64)>>,
}

/// Smallest on-tick pip value `>= raw`.
fn ceil_tick(raw: i64) -> i64 {
    let tick = i64::from(TICK);
    raw.div_euclid(tick) * tick + if raw.rem_euclid(tick) == 0 { 0 } else { tick }
}

/// A non-negative float as a share count (saturating; NaN and negatives are 0).
fn whole(x: f64) -> i64 {
    if x >= i64::MAX as f64 {
        i64::MAX
    } else if x > 0.0 {
        x as i64
    } else {
        0
    }
}

impl Sniper {
    /// Sniper with its config.
    pub fn new(cfg: SniperCfg) -> Self {
        Self {
            cfg,
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Whether the EXP-003 unwind has started: no buys from here, only covered sells.
    pub fn unwinding(&self, now_ns: i64) -> bool {
        self.cfg.unwind_from_utc.is_some_and(|t| now_ns >= to_ns(t))
    }

    /// Drop unwind sells older than an hour and return the trailing hour's proceeds.
    fn hourly_used(&self, now_ns: i64) -> f64 {
        let Ok(mut sent) = self.sent.lock() else {
            return f64::INFINITY;
        };
        sent.retain(|(at, _)| now_ns.saturating_sub(*at) < HOUR_NS);
        sent.iter().map(|(_, n)| n).sum()
    }

    fn record(&self, now_ns: i64, proceeds: f64) {
        if let Ok(mut sent) = self.sent.lock() {
            sent.push((now_ns, proceeds));
        }
    }

    /// The gate a snipe passes, for the unwind: verified, fresh fair, fresh book near fair.
    fn gate<'a>(
        &self,
        snap: &Snapshot<'a>,
        ex: &ExchangeId,
    ) -> Result<(&'a FairQuote, &'a L2Book), &'static str> {
        if !snap.markets.is_verified(ex) {
            return Err("unverified");
        }
        let fair = snap.fresh_fair(ex).ok_or("no fresh fair")?;
        let book = snap.fresh_book(ex).ok_or("no fresh book")?;
        let reference = book
            .mid()
            .or_else(|| {
                book.best_bid()
                    .or_else(|| book.best_ask())
                    .map(|l| l.price.prob())
            })
            .ok_or("empty book")?;
        if (reference - fair.p_yes).abs() > snap.fair_cfg.mapping_error_pts / 100.0 {
            return Err("mapping error suspected");
        }
        Ok((fair, book))
    }

    /// Why a held exchange that passed the gate is not in the unwind set, if it is not.
    fn outside_set(snap: &Snapshot<'_>, ex: &ExchangeId, side_fair: i64) -> Option<&'static str> {
        if side_fair < UNWIND_MIN_SIDE_FAIR_PIPS {
            Some("held side not near-certain")
        } else if snap.is_locked(ex) {
            Some("locked leg")
        } else if snap
            .intents
            .iter()
            .any(|i| &i.exchange_id == ex && i.strategy != NAME)
        {
            Some("another strategy is working it")
        } else if snap.has_live_intent(NAME, ex) {
            Some("already working it")
        } else if snap.needle.is_some_and(|n| {
            snap.markets
                .rule(ex)
                .is_some_and(|r| !r.race_id.is_empty() && n.posteriors.contains_key(&r.race_id))
        }) {
            // Election night: the executor buys and rolls these races on the needle's posteriors;
            // its filled buys must not be sold back here to top up cash.
            Some("the election-night executor's race")
        } else {
            None
        }
    }

    /// One unwind pass: covered sells of the unwind set (see the module docs).
    fn unwind(&self, snap: &Snapshot<'_>, d: &mut Decision) {
        let cfg = &self.cfg;
        let equity = snap.equity;
        let Some(cash) = snap.cash.filter(|_| equity > 0.0) else {
            d.note("sniper unwind: cash unknown, waiting");
            return;
        };
        let mut gap = (cfg.unwind_target_cash_frac * equity - cash) * PIPS;
        if gap <= 0.0 {
            d.note(format!(
                "sniper unwind: cash {cash:.2} at or above {} of equity, done",
                cfg.unwind_target_cash_frac
            ));
            return;
        }
        let mut hour_room = (cfg.unwind_max_hourly_frac * equity - self.hourly_used(snap.now_ns)) * PIPS;
        let per_order = cfg.max_mistake_frac * equity * PIPS;
        let haircut = (cfg.unwind_max_haircut_pts * 100.0).round() as i64;
        let mut held: Vec<(&ExchangeId, i64)> = snap
            .positions
            .iter()
            .filter(|(_, q)| **q != 0)
            .map(|(ex, q)| (ex, *q))
            .collect();
        held.sort_by(|a, b| a.0.cmp(b.0));
        for (ex, qty) in held {
            let (fair, book) = match self.gate(snap, ex) {
                Ok(v) => v,
                Err(why) => {
                    d.note(format!("sniper unwind {ex}: {why}, skipped"));
                    continue;
                }
            };
            let fair_pips = (fair.p_yes * PIPS).round() as i64;
            let side = if qty > 0 { Side::Yes } else { Side::No };
            let side_fair = match side {
                Side::Yes => fair_pips,
                Side::No => i64::from(PIPS_PER_UNIT) - fair_pips,
            };
            if let Some(why) = Self::outside_set(snap, ex, side_fair) {
                d.note(format!("sniper unwind {ex}: {why}, skipped"));
                continue;
            }
            let floor = ceil_tick(side_fair - haircut);
            let bid = book.touch(side, Action::Sell).map(|p| i64::from(p.get()));
            let limit = bid.map_or(floor, |b| b.max(floor));
            // A fair within the haircut of 1.0 can leave no valid price at or above the floor.
            let Some(price) = u32::try_from(limit).ok().and_then(|r| Pips::new(r).ok()) else {
                d.note(format!("sniper unwind {ex}: no valid price at {limit} pips"));
                continue;
            };
            let lim = limit as f64;
            if hour_room < lim {
                d.note(format!(
                    "sniper unwind: trailing-hour proceeds at {} of equity, pausing",
                    cfg.unwind_max_hourly_frac
                ));
                break;
            }
            let size = qty
                .abs()
                .min(whole((per_order / lim + 1e-9).floor()))
                .min(whole((gap / lim - 1e-9).ceil()))
                .min(whole((hour_room / lim + 1e-9).floor()));
            let draft = Draft {
                exchange: ex.clone(),
                side,
                action: Action::Sell,
                qty: size,
                price,
                fair_yes: Some(fair.p_yes),
            };
            let key = format!("{ex}:unwind");
            let Some(spec) = draft.into_spec(snap, NAME, &key, cfg.order_ttl_s, false) else {
                d.note(format!("sniper unwind {ex}: size < 1"));
                continue;
            };
            let proceeds = size as f64 * lim;
            d.note(format!(
                "sniper unwind {ex}: sell {side} x{size} @ {price} (fair {:.3}, floor {floor})",
                fair.p_yes
            ));
            d.event(
                UNWIND_EVENT,
                serde_json::json!({
                    "exchangeId": ex.as_str(),
                    "side": side.as_str(),
                    "quantity": size,
                    "limit": limit,
                    "fair": side_fair,
                    "floor": floor,
                    "bid": bid,
                    "haircutPips": side_fair - limit,
                    "cash": cash,
                    "equity": equity,
                }),
            );
            d.bundles.push(OrderBundle::single(spec));
            self.record(snap.now_ns, proceeds / PIPS);
            gap -= proceeds;
            hour_room -= proceeds;
            if gap <= 0.0 {
                break;
            }
        }
    }

    /// Cancel this exchange's resting snipes when the fair they were built on went stale.
    /// Returns true when the fair is stale (nothing new may be built on it).
    fn tend(&self, snap: &Snapshot<'_>, ex: &ExchangeId, fair: &FairQuote, d: &mut Decision) -> bool {
        if snap.is_fair_fresh(fair) {
            return false;
        }
        let mut cancelled = false;
        for i in snap.live_intents(NAME).filter(|i| &i.exchange_id == ex) {
            d.cancel(&i.intent_id, "stale fair");
            cancelled = true;
        }
        if cancelled {
            d.note(format!("sniper {ex}: stale fair, cancelling resting orders"));
        }
        true
    }

    fn snipe(&self, snap: &Snapshot<'_>, ex: &ExchangeId, fair: &FairQuote, d: &mut Decision) {
        if self.tend(snap, ex, fair, d) {
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
        if self.unwinding(snap.now_ns) {
            for (ex, fair) in snap.fair_sorted() {
                self.tend(snap, ex, fair, &mut d);
            }
            self.unwind(snap, &mut d);
            return d;
        }
        // The sniper's edge is a few points on near-certain outcomes held to settlement; until the
        // unwind it never sells. Left alone it fills every cap and starves the strategies that can actually win
        // (far quotes, the scenario book), so once the book is this full it only tends what it
        // has (stale-fair cancels) and adds nothing.
        let total = snap.total_exposure();
        let budget = self.cfg.max_total_gross_frac * snap.equity;
        let full = total >= budget;
        if full {
            d.note(format!(
                "sniper: gross budget full ({total:.2} >= {budget:.2} of {:.2} equity), not adding",
                snap.equity
            ));
        }
        for (ex, fair) in snap.fair_sorted() {
            if full {
                self.tend(snap, ex, fair, &mut d);
            } else {
                self.snipe(snap, ex, fair, &mut d);
            }
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

    /// The bridge's `test_stops_adding_once_the_book_is_full`: a plain snipe is on offer, but the
    /// account is already at the sniper's share of equity, so nothing is added and nothing sold.
    #[test]
    fn stops_adding_once_the_book_is_full() {
        let mut w = world();
        w.rule("2", "m2", "", "", true);
        w.fair("1", 0.60, None);
        w.book("1", &[(5000, 10)], &[(5500, 20)]);
        // 100 000 equity, 35 % budget = 35 000. Held: 40 000 YES at a 0.5 mark on exchange 2
        // (20 000) plus a resting NO buy of 1 000 at 0.40 (400) and 15 000 unmarked NO shares
        // valued at the full payout: comfortably over.
        w.position("2", 40_000);
        w.mark("2", 0.5);
        w.position("3", -15_000);
        let mut resting = intent("mm:2:ask:1", "mm", "2", Side::No, Action::Buy, 4000);
        resting.remaining = 1_000;
        w.intents.push(resting);
        let snap = w.snap();
        assert_eq!(snap.total_exposure(), 35_400.0);
        let d = sniper().decide(&snap);
        assert!(d.bundles.is_empty(), "{:?}", d.notes);
        assert!(d.cancels.is_empty(), "never sells or pulls what it holds");
        assert!(d.notes.iter().any(|n| n.contains("gross budget full")));

        // The cap is a share of equity: more equity reopens the book.
        w.equity = 200_000.0;
        assert_eq!(sniper().decide(&w.snap()).bundles.len(), 1);

        // A full book still cancels snipes whose fair went stale.
        w.equity = 100_000.0;
        w.fair_at("1", 0.60, None, w.now_ns - 61 * 1_000_000_000);
        w.intents
            .push(intent("sniper:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5650));
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 1);
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

    // ── EXP-003 unwind ──────────────────────────────────────────────────────────────────────

    use susq_core::config::SNIPER_UNWIND_FROM_S;

    /// One sell as `(exchange, side, quantity, price)`.
    type Sell = (String, String, i64, u32);

    fn sells(d: &Decision) -> Vec<Sell> {
        d.bundles
            .iter()
            .map(|b| {
                let l = &b.legs[0];
                assert_eq!(l.action, Action::Sell, "the unwind only sells");
                (
                    l.exchange_id.as_str().to_owned(),
                    l.side.to_string(),
                    l.quantity.get(),
                    l.price.get(),
                )
            })
            .collect()
    }

    fn sell(ex: &str, side: &str, qty: i64, price: u32) -> Sell {
        (ex.to_owned(), side.to_owned(), qty, price)
    }

    /// 100 000 equity, 20 000 cash, at the unwind start, holding 500 YES on "10" (fair 0.95,
    /// book 0.94 / 0.96).
    fn unwinding_world() -> World {
        let mut w = World::new(100_000.0);
        w.now_ns = SNIPER_UNWIND_FROM_S * NS_PER_S;
        w.cash = Some(20_000.0);
        w.rule("10", "m10", "", "", true);
        w.fair("10", 0.95, None);
        w.book("10", &[(9400, 1_000)], &[(9600, 1_000)]);
        w.position("10", 500);
        w
    }

    #[test]
    fn no_unwind_before_the_start() {
        let mut w = unwinding_world();
        w.now_ns -= NS_PER_S;
        assert!(!sniper().unwinding(w.now_ns));
        assert!(sniper().decide(&w.snap()).bundles.is_empty());
        let off = Sniper::new(SniperCfg {
            unwind_from_utc: None,
            ..SniperCfg::default()
        });
        assert!(off.decide(&unwinding_world().snap()).bundles.is_empty());
        assert_eq!(
            sells(&sniper().decide(&unwinding_world().snap())),
            vec![sell("10", "yes", 500, 9400)]
        );
    }

    #[test]
    fn sells_at_the_better_of_bid_and_fair_minus_haircut() {
        type Rungs<'a> = &'a [(u32, i64)];
        let cases: [(Rungs, Rungs, f64, u32); 5] = [
            (&[(9400, 10)], &[(9600, 10)], 0.95, 9400), // bid within 2 pts: take it
            (&[(9600, 10)], &[(9700, 10)], 0.95, 9600), // bid above fair: still the bid
            (&[(9200, 10)], &[(9600, 10)], 0.95, 9300), // bid too deep: rest at the floor
            (&[], &[(9600, 10)], 0.95, 9300),           // no bid: rest at the floor
            (&[(9000, 10)], &[(9600, 10)], 0.9537, 9350), // 0.9337 rounds toward fair
        ];
        for (bids, asks, fair, want) in cases {
            let mut w = unwinding_world();
            w.fair("10", fair, None);
            w.book("10", bids, asks);
            let d = sniper().decide(&w.snap());
            assert_eq!(sells(&d), vec![sell("10", "yes", 500, want)], "{:?}", d.notes);
            let leg = &d.bundles[0].legs[0];
            assert!(leg.intent_id.starts_with("sniper:10:unwind:"));
            assert_eq!(leg.fair_yes, Some(fair));
            assert!(!leg.needle_derived);
        }
        // A NO holding sells NO; its bid is 1 − the YES ask.
        let mut w = unwinding_world();
        w.positions.clear();
        w.rule("20", "m20", "", "", true);
        w.fair("20", 0.04, None);
        w.book("20", &[(300, 10)], &[(500, 10)]);
        w.position("20", -400);
        assert_eq!(
            sells(&sniper().decide(&w.snap())),
            vec![sell("20", "no", 400, 9500)]
        );
    }

    #[test]
    fn never_sells_below_fair_minus_haircut() {
        for fair in [0.90, 0.9123, 0.948, 0.9537, 0.999] {
            for bid in [None, Some(8000), Some(8900), Some(9250), Some(9450), Some(9950)] {
                for haircut in [0.0, 0.5, 2.0, 5.0] {
                    let mut w = unwinding_world();
                    w.fair("10", fair, None);
                    let bids: Vec<(u32, i64)> = bid.map(|b| (b, 10)).into_iter().collect();
                    let asks: &[(u32, i64)] = if bid == Some(9950) { &[] } else { &[(9950, 10)] };
                    w.book("10", &bids, asks);
                    let s = Sniper::new(SniperCfg {
                        unwind_max_haircut_pts: haircut,
                        ..SniperCfg::default()
                    });
                    let d = s.decide(&w.snap());
                    let side_fair = (fair * 10_000.0).round();
                    let floor = side_fair - haircut * 100.0;
                    let got = sells(&d);
                    if floor > 9950.0 && bid.is_none_or(|b| f64::from(b) < floor) {
                        // No valid price at or above the floor (0.995 is the top): nothing is sold.
                        assert!(got.is_empty(), "{:?}", d.notes);
                        continue;
                    }
                    assert_eq!(got.len(), 1, "{fair} {bid:?} {haircut}: {:?}", d.notes);
                    assert!(f64::from(got[0].3) >= floor, "{fair} {bid:?} {haircut}");
                    assert_eq!(got[0].3 % 50, 0);
                }
            }
        }
    }

    #[test]
    fn never_sells_more_than_held() {
        let mut w = unwinding_world();
        w.position("10", 3);
        assert_eq!(
            sells(&sniper().decide(&w.snap())),
            vec![sell("10", "yes", 3, 9400)]
        );
        // A big holding goes one per-order cap (1 % of equity in proceeds) at a time.
        w.position("10", 50_000);
        let got = sells(&sniper().decide(&w.snap()));
        assert_eq!(got, vec![sell("10", "yes", 1063, 9400)]);
        assert!(1063.0 * 0.94 <= 0.01 * w.equity);
    }

    #[test]
    fn never_touches_locked_legs_other_strategies_or_longshots() {
        let mut w = unwinding_world();
        w.target("10", 500, 0.95, true);
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("locked leg")));

        let mut w = unwinding_world();
        w.target("10", 500, 0.95, false);
        assert_eq!(
            sells(&sniper().decide(&w.snap())).len(),
            1,
            "only locked legs are kept"
        );

        let mut w = unwinding_world();
        w.intents.push(intent(
            "farquote:10:no:1",
            "farquote",
            "10",
            Side::No,
            Action::Buy,
            100,
        ));
        assert!(sniper().decide(&w.snap()).bundles.is_empty());

        let mut w = unwinding_world();
        w.intents.push(intent(
            "sniper:10:unwind:1",
            NAME,
            "10",
            Side::Yes,
            Action::Sell,
            9300,
        ));
        assert!(sniper().decide(&w.snap()).bundles.is_empty());

        let mut w = unwinding_world();
        w.position("10", -500); // NO at 0.05: a longshot leg, not the sniper's book
        assert!(sniper().decide(&w.snap()).bundles.is_empty());

        let mut w = unwinding_world();
        w.rule("10", "m10", "", "", false);
        assert!(sniper().decide(&w.snap()).bundles.is_empty(), "unverified");

        let mut w = unwinding_world();
        w.fair_at("10", 0.95, None, w.now_ns - 61 * NS_PER_S);
        assert!(sniper().decide(&w.snap()).bundles.is_empty(), "stale fair");
    }

    /// On election night the executor's filled buys sit on near-certain sides with no live
    /// intent left; the unwind must not sell them back to top up cash.
    #[test]
    fn leaves_the_election_night_executors_races_alone() {
        let mut w = unwinding_world();
        w.rule("10", "m10", "2026-SEN-PA", "DEM", true);
        w.needle(true, &[("2026-SEN-GA", 0.5, 0.5, 3)]);
        assert_eq!(sells(&sniper().decide(&w.snap())).len(), 1, "another race");
        w.needle(true, &[("2026-SEN-PA", 0.97, 0.03, 3)]);
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("election-night executor")));
    }

    #[test]
    fn stops_at_the_cash_target_and_waits_for_cash() {
        let mut w = unwinding_world();
        w.cash = Some(60_000.0);
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("done")));
        // The last sell only covers the gap: 500 short at 0.94 is 532 shares.
        w.cash = Some(59_500.0);
        w.position("10", 5_000);
        assert_eq!(
            sells(&sniper().decide(&w.snap())),
            vec![sell("10", "yes", 532, 9400)]
        );
        w.cash = None;
        assert!(sniper().decide(&w.snap()).bundles.is_empty());
    }

    #[test]
    fn hourly_cap_counts_what_it_sent_and_releases_after_an_hour() {
        let w = unwinding_world();
        let s = sniper();
        s.record(w.now_ns - 60 * NS_PER_S, 10_000.0);
        assert!(s.decide(&w.snap()).bundles.is_empty());
        let mut later = unwinding_world();
        later.now_ns = w.now_ns + 3_600 * NS_PER_S;
        later.fair("10", 0.95, None);
        later.book("10", &[(9400, 1_000)], &[(9600, 1_000)]);
        assert_eq!(sells(&s.decide(&later.snap())).len(), 1);
        // What it sends is counted: 500 × 0.94 = 470 of the 10 000 an hour.
        assert!((s.hourly_used(later.now_ns) - 470.0).abs() < 1e-9);
    }

    #[test]
    fn no_new_sniper_buys_after_the_start() {
        let mut w = unwinding_world();
        w.positions.clear();
        w.rule("1", "m1", "", "", true);
        w.fair("1", 0.60, None);
        w.book("1", &[(5000, 10)], &[(5500, 20)]); // a snipe before the start
        let mut before = w.clone();
        before.now_ns -= NS_PER_S;
        before.fair("1", 0.60, None);
        before.book("1", &[(5000, 10)], &[(5500, 20)]);
        assert_eq!(sniper().decide(&before.snap()).bundles.len(), 1);
        let d = sniper().decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", d.notes);
        // Stale fairs still cancel what it has resting.
        w.fair_at("1", 0.60, None, w.now_ns - 61 * NS_PER_S);
        w.intents
            .push(intent("sniper:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5650));
        assert_eq!(sniper().decide(&w.snap()).cancels.len(), 1);
    }

    #[test]
    fn journals_each_unwind_sell() {
        let d = sniper().decide(&unwinding_world().snap());
        let e = d.events.iter().find(|e| e.kind == UNWIND_EVENT).expect("event");
        assert_eq!(e.body["exchangeId"], "10");
        assert_eq!(e.body["limit"], 9400);
        assert_eq!(e.body["floor"], 9300);
        assert_eq!(e.body["haircutPips"], 100);
    }

    /// `tests/fixtures/sniper_unwind.json` is also run through the bridge's `plan_unwind`
    /// (`bridge_py/tests/test_sniper_unwind.py`): both traders must emit exactly these sells.
    #[test]
    fn parity_fixture_shared_with_the_bridge() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/sniper_unwind.json"))
                .expect("fixture parses");
        let cfg: SniperCfg = serde_json::from_value(fixture["config"].clone()).expect("config");
        assert_eq!(cfg, SniperCfg::default(), "the fixture uses the shipped values");
        let cases = fixture["cases"].as_array().expect("cases");
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let num = |k: &str| case[k].as_f64().expect(k);
            let mut w = World::new(num("equity"));
            w.now_ns = case["now_s"].as_i64().expect("now_s") * NS_PER_S;
            w.cash = case["cash"].as_f64();
            for (ex, p) in case["fair"].as_object().expect("fair") {
                w.rule(ex, &format!("m{ex}"), "", "", true);
                w.fair(ex, p.as_f64().expect("p"), None);
            }
            for (ex, b) in case["books"].as_object().expect("books") {
                let rung = |v: &serde_json::Value| -> Vec<(u32, i64)> {
                    v.as_u64().map(|p| (p as u32, 1_000)).into_iter().collect()
                };
                w.book(ex, &rung(&b[0]), &rung(&b[1]));
            }
            for (ex, q) in case["positions"].as_object().expect("positions") {
                w.position(ex, q.as_i64().expect("qty"));
            }
            for ex in case["locked"].as_array().expect("locked") {
                w.target(ex.as_str().expect("ex"), 1, 0.5, true);
            }
            for (n, i) in case["intents"].as_array().expect("intents").iter().enumerate() {
                let strategy = i["strategy"].as_str().expect("strategy");
                let ex = i["exchange"].as_str().expect("exchange");
                w.intents.push(intent(
                    &format!("{strategy}:{ex}:{n}"),
                    strategy,
                    ex,
                    Side::Yes,
                    Action::Buy,
                    5000,
                ));
            }
            let s = sniper();
            s.record(w.now_ns - NS_PER_S, num("hourly_used"));
            let want: Vec<Sell> = case["expect"]
                .as_array()
                .expect("expect")
                .iter()
                .map(|e| {
                    sell(
                        e["exchange"].as_str().expect("exchange"),
                        e["side"].as_str().expect("side"),
                        e["quantity"].as_i64().expect("quantity"),
                        e["price"].as_u64().expect("price") as u32,
                    )
                })
                .collect();
            let d = s.decide(&w.snap());
            assert_eq!(sells(&d), want, "{name}: {:?}", d.notes);
        }
    }
}
