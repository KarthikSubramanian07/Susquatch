//! Market maker: two-sided quotes around a fresh fair value (disabled unless the decision table says so).
//!
//! * Spread: `max(2 ticks, sponsor_spread + 1 tick)`, where the sponsor spread is the book spread with
//!   our own quotes removed. Our quotes are inferred from live `mm` intents: at a price where we rest,
//!   our remaining size is subtracted from the level, and the level only counts if someone else is left.
//!   With one side empty after that there is no sponsor spread and the 2-tick floor applies.
//! * Skew (ticks, added to both quotes): `−(position / inventory_unit) × inventory_skew_ticks
//!   − max cluster utilisation × cluster_skew_ticks × sign(position)`; quotes lean against inventory
//!   and harder when the exchange's clusters are loaded.
//! * bid = floor(fair − half + skew), ask = ceil(fair + half + skew). A side that would cross the book
//!   (bid ≥ best ask, ask ≤ best bid) is not quoted: post-only by construction.
//! * Inventory: no bid at `position ≥ max_inventory`, no ask at `position ≤ −max_inventory`, and quote
//!   sizes never carry the position past the limit.
//! * The ask is always sent as a NO buy at `1 − ask`, never a naked YES sell.
//! * A real-money move `|p_yes − prev_p_yes| ≥ cancel_move_pts` on any fresh fair cancels every mm quote
//!   on every exchange and quotes nothing that tick (stale fairs are excluded: an old move is not news).
//! * A stale fair cancels the quotes on that exchange; a quote whose price differs from the new target
//!   is cancelled and replaced.
//! * Mapping-error gate: a book mid more than `fair.mapping_error_pts` from fair means the fair is
//!   probably mapped to the wrong contract; quotes on that exchange are cancelled and not replaced.

use susq_book::{L2Book, Level};
use susq_core::{
    Action, ExchangeId, OrderBundle, Pips, Side, config::MmCfg, price::PIPS_PER_UNIT, price::TICK,
};

use crate::{
    Strategy,
    draft::{Draft, intent_id},
    snapshot::{Decision, FairQuote, IntentView, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "mm";

/// Minimum quoted spread in ticks.
const MIN_SPREAD_TICKS: i64 = 2;

/// The market maker.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketMaker {
    pub cfg: MmCfg,
}

/// Which of our two quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteSide {
    /// YES bid (buy YES).
    Bid,
    /// YES ask, sent as buy NO at the complement.
    Ask,
}

impl QuoteSide {
    const fn key(self) -> &'static str {
        match self {
            QuoteSide::Bid => "bid",
            QuoteSide::Ask => "ask",
        }
    }

    const fn order_side(self) -> Side {
        match self {
            QuoteSide::Bid => Side::Yes,
            QuoteSide::Ask => Side::No,
        }
    }

    fn of(intent: &IntentView) -> Option<QuoteSide> {
        match (intent.side, intent.action) {
            (Side::Yes, Action::Buy) => Some(QuoteSide::Bid),
            (Side::No, Action::Buy) => Some(QuoteSide::Ask),
            _ => None,
        }
    }
}

/// Target quote prices (YES space) and sizes for one exchange.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quotes {
    pub bid: Option<(Pips, i64)>,
    pub ask: Option<(Pips, i64)>,
    pub spread_ticks: i64,
    pub skew_ticks: f64,
}

impl MarketMaker {
    /// Market maker with its config.
    pub fn new(cfg: MmCfg) -> Self {
        Self { cfg }
    }

    /// Target quotes for one exchange given its fair, book, our resting quotes and position.
    pub fn quotes(
        &self,
        fair: f64,
        book: &L2Book,
        ours: &[&IntentView],
        position: i64,
        cluster_util: f64,
    ) -> Quotes {
        let resting = |q: QuoteSide| {
            ours.iter()
                .filter(move |i| QuoteSide::of(i) == Some(q))
                .map(|i| match q {
                    QuoteSide::Bid => (i.price, i.remaining),
                    QuoteSide::Ask => (i.price.complement(), i.remaining),
                })
                .collect::<Vec<_>>()
        };
        let other_bid = best_excluding(&book.bids, &resting(QuoteSide::Bid));
        let other_ask = best_excluding(&book.asks, &resting(QuoteSide::Ask));
        let sponsor = match (other_bid, other_ask) {
            (Some(b), Some(a)) => Some(i64::from(a.ticks()) - i64::from(b.ticks())),
            _ => None,
        };
        let spread_ticks = sponsor.map_or(MIN_SPREAD_TICKS, |s| (s + 1).max(MIN_SPREAD_TICKS));

        let inventory = if self.cfg.inventory_unit > 0 {
            position as f64 / self.cfg.inventory_unit as f64 * self.cfg.inventory_skew_ticks
        } else {
            0.0
        };
        let cluster = cluster_util.max(0.0) * self.cfg.cluster_skew_ticks * position.signum() as f64;
        let skew_ticks = -inventory - cluster;

        let tick = f64::from(TICK) / f64::from(PIPS_PER_UNIT);
        let half = spread_ticks as f64 / 2.0 * tick;
        let bid = Pips::floor_from_prob(fair - half + skew_ticks * tick)
            .filter(|b| book.best_ask().is_none_or(|a| *b < a.price));
        let ask = Pips::ceil_from_prob(fair + half + skew_ticks * tick)
            .filter(|a| book.best_bid().is_none_or(|b| *a > b.price));
        let (bid, ask) = match (bid, ask) {
            (Some(b), Some(a)) if b >= a => (None, None),
            other => other,
        };

        let max = self.cfg.max_inventory;
        let bid_qty = self.cfg.quote_qty.min(max.saturating_sub(position));
        let ask_qty = self.cfg.quote_qty.min(max.saturating_add(position));
        Quotes {
            bid: bid.filter(|_| bid_qty >= 1).map(|p| (p, bid_qty)),
            ask: ask.filter(|_| ask_qty >= 1).map(|p| (p, ask_qty)),
            spread_ticks,
            skew_ticks,
        }
    }

    fn quote_exchange(&self, snap: &Snapshot<'_>, ex: &ExchangeId, fair: &FairQuote, d: &mut Decision) {
        let ours: Vec<&IntentView> = snap.live_intents(NAME).filter(|i| &i.exchange_id == ex).collect();
        if !snap.is_fair_fresh(fair) {
            for i in &ours {
                d.cancel(&i.intent_id, "stale fair");
            }
            return;
        }
        if !snap.markets.is_verified(ex) {
            return;
        }
        let Some(book) = snap.fresh_book(ex) else {
            d.note(format!("mm {ex}: no fresh book, quotes left as they are"));
            return;
        };
        if let Some(mid) = book
            .mid()
            .filter(|m| (m - fair.p_yes).abs() > snap.fair_cfg.mapping_error_pts / 100.0)
        {
            for i in &ours {
                d.cancel(&i.intent_id, "mapping error suspected");
            }
            d.note(format!(
                "mm {ex}: mapping error suspected (book {mid:.3} vs fair {:.3}), not quoting",
                fair.p_yes
            ));
            return;
        }
        let util = snap
            .markets
            .clusters(ex)
            .iter()
            .filter_map(|c| snap.cluster_utilization.get(c).copied())
            .fold(0.0_f64, f64::max);
        let q = self.quotes(fair.p_yes, book, &ours, snap.position(ex), util);
        for (side, target) in [(QuoteSide::Bid, q.bid), (QuoteSide::Ask, q.ask)] {
            self.reconcile(snap, ex, fair.p_yes, side, target, &ours, d);
        }
    }

    /// Keep a resting quote that already matches the target; otherwise cancel and replace it.
    #[allow(clippy::too_many_arguments)]
    fn reconcile(
        &self,
        snap: &Snapshot<'_>,
        ex: &ExchangeId,
        fair: f64,
        side: QuoteSide,
        target: Option<(Pips, i64)>,
        ours: &[&IntentView],
        d: &mut Decision,
    ) {
        let order_side = side.order_side();
        let target = target.map(|(yes_price, qty)| {
            let price = match side {
                QuoteSide::Bid => yes_price,
                QuoteSide::Ask => yes_price.complement(),
            };
            (price, qty)
        });
        let resting: Vec<&&IntentView> = ours.iter().filter(|i| QuoteSide::of(i) == Some(side)).collect();
        if let ([only], Some((price, _))) = (resting.as_slice(), target)
            && only.price == price
        {
            return;
        }
        let reason = if target.is_some() {
            "requote"
        } else {
            "side withdrawn"
        };
        for i in &resting {
            d.cancel(&i.intent_id, reason);
        }
        let Some((price, qty)) = target else {
            return;
        };
        let key = format!("{ex}:{}", side.key());
        let id = intent_id(NAME, &key, snap.now_s());
        if resting.iter().any(|i| i.intent_id == id) {
            // Replaced within the same second: the new id would collide; requote next second.
            return;
        }
        let draft = Draft {
            exchange: ex.clone(),
            side: order_side,
            action: Action::Buy,
            qty,
            price,
            fair_yes: Some(fair),
        };
        if let Some(spec) = draft.into_spec(snap, NAME, &key, self.cfg.order_ttl_s, false) {
            d.bundles.push(OrderBundle::single(spec));
        }
    }
}

/// Best price on one side of the book after removing our own resting size `(yes price, qty)`.
fn best_excluding(levels: &[Level], ours: &[(Pips, i64)]) -> Option<Pips> {
    levels
        .iter()
        .find(|l| {
            let mine: i64 = ours.iter().filter(|(p, _)| *p == l.price).map(|(_, q)| q).sum();
            l.qty > mine
        })
        .map(|l| l.price)
}

impl Strategy for MarketMaker {
    fn name(&self) -> &'static str {
        NAME
    }

    fn decide(&self, snap: &Snapshot<'_>) -> Decision {
        let mut d = Decision::default();
        if !self.cfg.enabled {
            return d;
        }
        let threshold = self.cfg.cancel_move_pts / 100.0;
        let mover = snap.fair_sorted().into_iter().find_map(|(ex, f)| {
            let moved = f
                .moved()
                .filter(|m| snap.is_fair_fresh(f) && m.abs() + 1e-12 >= threshold)?;
            Some((ex, moved))
        });
        if let Some((ex, moved)) = mover {
            for i in snap.live_intents(NAME) {
                d.cancel(&i.intent_id, format!("real-money move on {ex}"));
            }
            d.note(format!(
                "mm: fair on {ex} moved {moved:+.3}, all quotes cancelled"
            ));
            return d;
        }
        for (ex, fair) in snap.fair_sorted() {
            self.quote_exchange(snap, ex, fair, &mut d);
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{World, intent};

    fn cfg() -> MmCfg {
        MmCfg {
            enabled: true,
            ..MmCfg::default()
        }
    }

    fn world() -> World {
        let mut w = World::new(100_000.0);
        w.rule("1", "m1", "", "", true);
        w.rule_with("1", |r| r.clusters = vec!["national".into(), "state:OH".into()]);
        w
    }

    fn legs(d: &Decision) -> Vec<(Side, u32, i64)> {
        d.bundles
            .iter()
            .flat_map(|b| &b.legs)
            .map(|l| (l.side, l.price.get(), l.quantity.get()))
            .collect()
    }

    #[test]
    fn disabled_returns_an_empty_decision() {
        let mut w = world();
        w.fair("1", 0.5, Some(0.40));
        w.book("1", &[(4000, 10)], &[(6000, 10)]);
        w.intents
            .push(intent("mm:1:bid:1", NAME, "1", Side::Yes, Action::Buy, 4500));
        assert_eq!(
            MarketMaker::new(MmCfg::default()).decide(&w.snap()),
            Decision::default()
        );
    }

    #[test]
    fn spread_is_sponsor_spread_plus_one_tick() {
        let mut w = world();
        w.fair("1", 0.50, None);
        // Sponsor 0.48 / 0.52 = 8 ticks → quote 9 ticks: half 4.5 → bid floor(0.4775) = 0.475,
        // ask ceil(0.5225) = 0.525 (NO buy at 0.475).
        w.book("1", &[(4800, 100)], &[(5200, 100)]);
        let d = MarketMaker::new(cfg()).decide(&w.snap());
        assert_eq!(legs(&d), vec![(Side::Yes, 4750, 10), (Side::No, 4750, 10)]);
        assert!(d.bundles.iter().all(|b| b.legs[0].action == Action::Buy));
    }

    #[test]
    fn tight_sponsor_uses_the_two_tick_floor_and_never_crosses() {
        let mut w = world();
        let mm = MarketMaker::new(cfg());
        // 1-tick sponsor (0.495 / 0.50): spread max(2, 1 + 1) = 2 → bid 0.495, ask 0.505.
        w.book("1", &[(4950, 100)], &[(5000, 100)]);
        let b = w.books.get(&ExchangeId::new("1")).expect("book").clone();
        let q = mm.quotes(0.50, &b, &[], 0, 0.0);
        assert_eq!(q.spread_ticks, 2);
        assert_eq!(q.bid.map(|b| b.0.get()), Some(4950));
        assert_eq!(q.ask.map(|a| a.0.get()), Some(5050));
        // Fair far above the book: the bid would cross the 0.50 ask, so only the ask is quoted.
        let q = mm.quotes(0.60, &b, &[], 0, 0.0);
        assert_eq!(q.bid, None);
        assert!(q.ask.is_some_and(|(a, _)| a.get() > 4950));
        let q = mm.quotes(0.40, &b, &[], 0, 0.0);
        assert_eq!(q.ask, None);
        assert!(q.bid.is_some_and(|(p, _)| p.get() < 5000));
    }

    #[test]
    fn skew_leans_against_inventory_and_loaded_clusters() {
        let w = {
            let mut w = world();
            w.book("1", &[(4000, 100)], &[(6000, 100)]);
            w
        };
        let b = w.books.get(&ExchangeId::new("1")).expect("book").clone();
        let mm = MarketMaker::new(cfg());
        let flat = mm.quotes(0.50, &b, &[], 0, 0.0);
        let long = mm.quotes(0.50, &b, &[], 100, 0.0);
        let long_loaded = mm.quotes(0.50, &b, &[], 100, 1.0);
        let short = mm.quotes(0.50, &b, &[], -100, 0.0);
        assert!((long.skew_ticks + 2.0).abs() < 1e-9);
        assert!((long_loaded.skew_ticks + 4.0).abs() < 1e-9);
        assert!(
            long.bid.expect("bid").0 < flat.bid.expect("bid").0,
            "long lowers the bid"
        );
        assert!(
            long.ask.expect("ask").0 < flat.ask.expect("ask").0,
            "long lowers the ask"
        );
        assert!(long_loaded.ask.expect("ask").0 < long.ask.expect("ask").0);
        assert!(
            short.bid.expect("bid").0 > flat.bid.expect("bid").0,
            "short raises the bid"
        );
    }

    #[test]
    fn inventory_limits_block_the_growing_side() {
        let mut w = world();
        w.book("1", &[(4000, 100)], &[(6000, 100)]);
        let b = w.books.get(&ExchangeId::new("1")).expect("book").clone();
        let mm = MarketMaker::new(cfg());
        let q = mm.quotes(0.50, &b, &[], 500, 0.0);
        assert!(q.bid.is_none() && q.ask.is_some());
        let q = mm.quotes(0.50, &b, &[], -500, 0.0);
        assert!(q.ask.is_none() && q.bid.is_some());
        let q = mm.quotes(0.50, &b, &[], 495, 0.0);
        assert_eq!(q.bid.map(|b| b.1), Some(5), "size stops at the limit");
    }

    #[test]
    fn our_own_quotes_are_excluded_from_the_sponsor_spread() {
        let mut w = world();
        w.fair("1", 0.50, None);
        // Book 0.475 / 0.525 is just us (10 each); the sponsor sits at 0.45 / 0.55 (20 ticks).
        w.book("1", &[(4750, 10), (4500, 50)], &[(5250, 10), (5500, 50)]);
        w.intents
            .push(intent("mm:1:bid:1", NAME, "1", Side::Yes, Action::Buy, 4750));
        w.intents
            .push(intent("mm:1:ask:1", NAME, "1", Side::No, Action::Buy, 4750));
        let d = MarketMaker::new(cfg()).decide(&w.snap());
        // Spread 21 ticks: bid floor(0.4475) = 0.445, ask ceil(0.5525) = 0.555 → both requoted.
        assert_eq!(legs(&d), vec![(Side::Yes, 4450, 10), (Side::No, 4450, 10)]);
        assert_eq!(d.cancels.len(), 2);
        assert!(d.cancels.iter().all(|c| c.reason == "requote"));
    }

    #[test]
    fn matching_quotes_are_left_alone() {
        let mut w = world();
        w.fair("1", 0.50, None);
        w.book("1", &[(4800, 100)], &[(5200, 100)]);
        w.intents
            .push(intent("mm:1:bid:1", NAME, "1", Side::Yes, Action::Buy, 4750));
        w.intents
            .push(intent("mm:1:ask:1", NAME, "1", Side::No, Action::Buy, 4750));
        assert!(MarketMaker::new(cfg()).decide(&w.snap()).is_empty());
    }

    #[test]
    fn a_two_point_real_money_move_cancels_every_quote() {
        let mut w = world();
        w.rule("2", "m2", "", "", true);
        w.fair("1", 0.50, Some(0.50));
        w.fair("2", 0.62, Some(0.60));
        w.book("1", &[(4800, 100)], &[(5200, 100)]);
        w.intents
            .push(intent("mm:1:bid:1", NAME, "1", Side::Yes, Action::Buy, 4750));
        w.intents
            .push(intent("mm:3:ask:1", NAME, "3", Side::No, Action::Buy, 4000));
        w.intents.push(intent(
            "sniper:1:yes:1",
            "sniper",
            "1",
            Side::Yes,
            Action::Buy,
            4000,
        ));
        let d = MarketMaker::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        let ids: Vec<&str> = d.cancels.iter().map(|c| c.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["mm:1:bid:1", "mm:3:ask:1"]);

        w.fair("2", 0.615, Some(0.60));
        assert!(
            MarketMaker::new(cfg()).decide(&w.snap()).cancels.is_empty(),
            "1.5 pts is not a move"
        );
    }

    #[test]
    fn mapping_error_gate_cancels_and_stops_quoting() {
        let mut w = world();
        w.fair("1", 0.20, None); // book mid 0.50: 30 pts away
        w.book("1", &[(4800, 100)], &[(5200, 100)]);
        w.intents
            .push(intent("mm:1:bid:1", NAME, "1", Side::Yes, Action::Buy, 1900));
        let d = MarketMaker::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 1);
        assert_eq!(d.cancels[0].reason, "mapping error suspected");
    }

    #[test]
    fn stale_fair_cancels_quotes_on_that_exchange() {
        let mut w = world();
        w.fair_at("1", 0.50, None, w.now_ns - 120 * 1_000_000_000);
        w.book("1", &[(4800, 100)], &[(5200, 100)]);
        w.intents
            .push(intent("mm:1:bid:1", NAME, "1", Side::Yes, Action::Buy, 4750));
        let d = MarketMaker::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 1);
        assert_eq!(d.cancels[0].reason, "stale fair");
    }
}
