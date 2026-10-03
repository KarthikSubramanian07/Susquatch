//! Far quoter: rest tiny-priced orders far from fair on near-certain outcomes, and wait.
//!
//! Day one showed where the leaderboard's money came from: accounts that paid 0.95 to 0.995 for
//! ~1 % outcomes (and sold 99 % outcomes near 0.005), with the whole balance, in the opening hours.
//! Whoever had resting orders at those prices was paid for them. The far quoter is the legitimate,
//! passive version of that: on every verified market whose fresh fair value is at or below
//! `max_fair` it rests BUY NO at NO prices `levels` (0.01 .. 0.10, i.e. selling YES at 0.99 .. 0.90),
//! and on every market whose fair is at or above `1 - max_fair` it rests BUY YES at those same
//! prices. A fill hands us a share worth ~0.99 for a few cents; the only cost of resting is the cash
//! the platform reserves, capped at `reserve_frac` of equity in total and `max_market_frac` per
//! market, with the reserve spread evenly over every market in the band (thin everywhere beats
//! deep on the first few).
//!
//! Every ladder is attributed to [`NAME`] in the intent log, so a restart recognises what already
//! rests instead of placing it again; duplicate levels, levels no longer wanted and orders far
//! bigger than today's sizing are pulled. Orders carry no fair value on purpose: they sit 85+ points
//! through fair by design, and the glitch cap (a budget for taker fills against broken quotes) would
//! refuse the whole ladder. The reserve and per-market budgets are the far quoter's limits; every
//! other gate (lease, kill, freeze, caps, buying power) still applies in the gateway.
//!
//! Unlike the sniper, a stale or missing fair does not pull a ladder: there is no verdict, so what
//! rests stays. Only a fresh fair that left the band, an unverified market, or a Cup price that
//! contradicts the fair (a suspected wrong mapping) does.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use susq_book::L2Book;
use susq_core::{
    Action, BookSide, ExchangeId, OrderBundle, Pips, Side, config::FarQuoteCfg, price::PIPS_PER_UNIT,
    side::yes_space,
};

use crate::{
    Strategy,
    draft::{Draft, shares_for_budget},
    snapshot::{Decision, IntentView, Snapshot},
};

/// Strategy name (intent id prefix and intent-log attribution).
pub const NAME: &str = "farquote";
/// Journal event kind of the per-scan summary.
pub const SCAN_EVENT: &str = "farquote_scan";
/// A resting order this many times bigger than today's sizing is re-placed at the new size.
const OVERSIZED: f64 = 1.5;

/// Why a market gets no ladder right now (the bridge's `fair_check` reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// No human has verified the Cup-to-reference mapping.
    Unverified,
    /// The fair file has no value for the market.
    NoFair,
    /// The fair value is older than `fair.stale_after_s`.
    Stale,
    /// No Cup book to sanity-check the fair against.
    NoCupPrice,
    /// The Cup price is farther than `fair.mapping_error_pts` from fair: probably a wrong mapping.
    MappingError,
}

impl Hold {
    /// Journal label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Hold::Unverified => "unverified",
            Hold::NoFair => "no_fair",
            Hold::Stale => "stale",
            Hold::NoCupPrice => "no_cup_price",
            Hold::MappingError => "mapping_error",
        }
    }

    /// True when the fair is merely missing or stale (keep what rests), false when the market is
    /// wrong (pull the ladder).
    pub const fn keeps_ladder(self) -> bool {
        matches!(self, Hold::NoFair | Hold::Stale | Hold::NoCupPrice)
    }
}

/// One market's verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Band {
    /// Near-certain: rest on `side` at the ladder prices.
    In { side: Side, fair_yes: f64 },
    /// Verified, fresh and priced, but not near-certain: pull any ladder.
    Out,
    /// No ladder, for the given reason.
    Hold(Hold),
}

/// One rung of the ladder a market should have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    /// What we buy: NO on a ~0 outcome, YES on a ~1 outcome.
    pub side: Side,
    /// Side-relative price: what one share costs.
    pub price: Pips,
    pub qty: i64,
}

impl Quote {
    /// The YES-space level the rung rests at.
    fn level(&self) -> (BookSide, Pips) {
        yes_space(self.side, Action::Buy, self.price)
    }

    /// Cash the rung reserves (SUSQies).
    fn cost(&self) -> f64 {
        self.qty as f64 * self.price.prob()
    }
}

/// The `farquote_scan` journal summary (same keys as the bridge's).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    /// Markets whose fair is near-certain right now.
    pub in_band: usize,
    /// Rungs those markets should have.
    pub wanted: usize,
    /// Rungs already resting.
    pub resting: usize,
    /// Rungs placed this scan.
    pub placed: usize,
    /// Resting orders pulled this scan.
    pub cancelled: usize,
    /// Cash the ladders tie up after this scan (SUSQies).
    pub reserved: f64,
    /// `reserve_frac` of equity.
    pub budget: f64,
    pub equity: f64,
    /// Reason → how many markets or rungs it held back.
    pub blocked: BTreeMap<String, u32>,
}

impl ScanSummary {
    fn block(&mut self, reason: &str) {
        *self.blocked.entry(reason.to_owned()).or_default() += 1;
    }
}

/// The far quoter.
#[derive(Debug, Clone, PartialEq)]
pub struct FarQuoter {
    pub cfg: FarQuoteCfg,
    /// `cfg.levels` as valid prices (config validation guarantees every level is one).
    levels: Vec<Pips>,
}

/// The Cup's view of a market: the mid, or the one touch it has.
fn cup_price(book: &L2Book) -> Option<f64> {
    book.mid().or_else(|| {
        book.best_bid()
            .or_else(|| book.best_ask())
            .map(|l| l.price.prob())
    })
}

/// Whole pips of a probability, the grid the band edge is judged on.
fn whole_pips(p: f64) -> i64 {
    (p * f64::from(PIPS_PER_UNIT)).round() as i64
}

fn cents(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

impl FarQuoter {
    /// Far quoter with its config.
    pub fn new(cfg: FarQuoteCfg) -> Self {
        let levels = cfg
            .levels
            .iter()
            .filter_map(|p| Pips::from_f64(*p).ok())
            .collect();
        Self { cfg, levels }
    }

    /// Which side to rest on for a fair YES probability, or `None` when the market is not
    /// near-certain. Judged on whole pips, like the bridge.
    pub fn side_for(&self, fair_yes: f64) -> Option<Side> {
        let edge = whole_pips(self.cfg.max_fair);
        let fair = whole_pips(fair_yes);
        if fair <= edge {
            Some(Side::No)
        } else if fair >= i64::from(PIPS_PER_UNIT) - edge {
            Some(Side::Yes)
        } else {
            None
        }
    }

    /// Judge one market: verified, fresh, sane against the Cup price, and near-certain.
    pub fn band(&self, snap: &Snapshot<'_>, ex: &ExchangeId) -> Band {
        if !snap.markets.is_verified(ex) {
            return Band::Hold(Hold::Unverified);
        }
        let Some(fair) = snap.fair.get(ex) else {
            return Band::Hold(Hold::NoFair);
        };
        if !snap.is_fair_fresh(fair) {
            return Band::Hold(Hold::Stale);
        }
        let Some(cup) = snap.books.get(ex).and_then(cup_price) else {
            return Band::Hold(Hold::NoCupPrice);
        };
        if (cup - fair.p_yes).abs() > snap.fair_cfg.mapping_error_pts / 100.0 {
            return Band::Hold(Hold::MappingError);
        }
        match self.side_for(fair.p_yes) {
            Some(side) => Band::In {
                side,
                fair_yes: fair.p_yes,
            },
            None => Band::Out,
        }
    }

    /// The ladder a market in the band should have right now. The reserve is spread evenly over
    /// every market in the band, then capped per market, then split evenly over the levels.
    pub fn wanted(&self, side: Side, equity: f64, markets_in_band: usize) -> Vec<Quote> {
        let spread = self.cfg.reserve_frac * equity / markets_in_band.max(1) as f64;
        let market_budget = (self.cfg.max_market_frac * equity).min(spread);
        let per_level = market_budget / self.levels.len().max(1) as f64;
        self.levels
            .iter()
            .filter_map(|&price| {
                let qty = shares_for_budget(per_level, price);
                (qty >= 1).then_some(Quote { side, price, qty })
            })
            .collect()
    }

    /// Pull every resting rung of a market that should have none.
    fn pull(have: &[&IntentView], why: &str, reserved: &mut f64, s: &mut ScanSummary, d: &mut Decision) {
        for o in have {
            d.cancel(&o.intent_id, why);
            *reserved -= o.resting_notional();
            s.cancelled += 1;
        }
    }

    /// Reconcile one in-band market's resting rungs with the ladder it should have.
    #[allow(clippy::too_many_arguments)]
    fn ladder(
        &self,
        snap: &Snapshot<'_>,
        ex: &ExchangeId,
        side: Side,
        fair_yes: f64,
        have: &[&IntentView],
        in_band: usize,
        reserved: &mut f64,
        s: &mut ScanSummary,
        d: &mut Decision,
    ) {
        let quotes = self.wanted(side, snap.equity, in_band);
        let wanted_qty: HashMap<(BookSide, Pips), i64> = quotes.iter().map(|q| (q.level(), q.qty)).collect();
        let mut seen: HashSet<(BookSide, Pips)> = HashSet::new();
        for o in have {
            let level = o.yes_level();
            // Duplicates (ladders re-placed across restarts), levels no longer wanted, and orders far
            // bigger than today's sizing (ladders placed before the budget was spread) go.
            let verdict = match wanted_qty.get(&level) {
                None => Some("level no longer wanted"),
                Some(_) if seen.contains(&level) => Some("duplicate level"),
                Some(&want) if o.remaining as f64 > OVERSIZED * want as f64 => {
                    Some("oversized for today's budget")
                }
                Some(_) => None,
            };
            match verdict {
                Some(why) => {
                    d.cancel(&o.intent_id, why);
                    *reserved -= o.resting_notional();
                    s.cancelled += 1;
                }
                None => {
                    seen.insert(level);
                }
            }
        }
        let budget = self.cfg.reserve_frac * snap.equity;
        let mut placed = 0;
        for q in &quotes {
            s.wanted += 1;
            if seen.contains(&q.level()) {
                s.resting += 1;
                continue;
            }
            let cost = q.cost();
            if *reserved + cost > budget + 1e-9 {
                s.block("reserve_budget");
                continue;
            }
            let draft = Draft {
                exchange: ex.clone(),
                side,
                action: Action::Buy,
                qty: q.qty,
                price: q.price,
                fair_yes: None,
            };
            let key = format!("{ex}:{side}:{}", q.price.get());
            let Some(spec) = draft.into_spec(snap, NAME, &key, self.cfg.order_ttl_s, false) else {
                continue;
            };
            d.bundles.push(OrderBundle::single(spec));
            *reserved += cost;
            s.placed += 1;
            placed += 1;
        }
        if placed > 0 {
            d.note(format!(
                "farquote {ex}: buy {side} ladder, {placed} of {} levels placed (fair {fair_yes:.3})",
                quotes.len()
            ));
        }
    }
}

impl Strategy for FarQuoter {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Top up every near-certain market's ladder; pull quotes from markets that left the band.
    fn decide(&self, snap: &Snapshot<'_>) -> Decision {
        let mut d = Decision::default();
        if !self.cfg.enabled {
            return d;
        }
        let mut exchanges: Vec<&ExchangeId> = snap.markets.exchanges().collect();
        exchanges.sort();
        let bands: Vec<(&ExchangeId, Band)> = exchanges
            .into_iter()
            .map(|ex| (ex, self.band(snap, ex)))
            .collect();
        let in_band = bands.iter().filter(|(_, b)| matches!(b, Band::In { .. })).count();
        let mut by_exchange: HashMap<&ExchangeId, Vec<&IntentView>> = HashMap::new();
        for i in snap.live_intents(NAME) {
            by_exchange.entry(&i.exchange_id).or_default().push(i);
        }
        let mut reserved: f64 = snap.live_intents(NAME).map(IntentView::resting_notional).sum();
        let mut s = ScanSummary {
            in_band,
            ..ScanSummary::default()
        };
        for (ex, band) in bands {
            let have = by_exchange.remove(ex).unwrap_or_default();
            match band {
                Band::In { side, fair_yes } => {
                    self.ladder(
                        snap,
                        ex,
                        side,
                        fair_yes,
                        &have,
                        in_band,
                        &mut reserved,
                        &mut s,
                        &mut d,
                    );
                }
                Band::Out => {
                    if !have.is_empty() {
                        d.note(format!(
                            "farquote {ex}: fair left the band, pulling {} rungs",
                            have.len()
                        ));
                    }
                    Self::pull(&have, "fair left the band", &mut reserved, &mut s, &mut d);
                }
                Band::Hold(hold) => {
                    s.block(hold.as_str());
                    if hold.keeps_ladder() {
                        continue; // no verdict: leave the ladder resting
                    }
                    if !have.is_empty() {
                        d.note(format!(
                            "farquote {ex}: {}, pulling {} rungs",
                            hold.as_str(),
                            have.len()
                        ));
                    }
                    Self::pull(&have, hold.as_str(), &mut reserved, &mut s, &mut d);
                }
            }
        }
        // Ladders on markets that dropped out of the map entirely are pulled too.
        let mut orphaned: Vec<(&ExchangeId, Vec<&IntentView>)> = by_exchange.into_iter().collect();
        orphaned.sort_by(|a, b| a.0.cmp(b.0));
        for (ex, have) in orphaned {
            d.note(format!(
                "farquote {ex}: no longer mapped, pulling {} rungs",
                have.len()
            ));
            s.block(Hold::Unverified.as_str());
            Self::pull(&have, "no longer mapped", &mut reserved, &mut s, &mut d);
        }
        s.reserved = cents(reserved);
        s.budget = cents(self.cfg.reserve_frac * snap.equity);
        s.equity = cents(snap.equity);
        d.event(SCAN_EVENT, serde_json::to_value(&s).unwrap_or_default());
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{NOW_S, World, intent};
    use susq_core::OrderSpec;

    const EQUITY: f64 = 100_000.0;

    fn quoter() -> FarQuoter {
        FarQuoter::new(FarQuoteCfg {
            enabled: true,
            ..FarQuoteCfg::default()
        })
    }

    /// One verified market, a 0.01 / 0.03 book (the Cup agrees the outcome is near zero).
    fn near_zero(ex: &str) -> World {
        let mut w = World::new(EQUITY);
        w.rule(ex, &format!("m{ex}"), "", "", true);
        w.book(ex, &[(100, 50)], &[(300, 50)]);
        w.fair(ex, 0.02, None);
        w
    }

    /// Rest a decision's orders in the world as live intents (what the OMS would show next scan).
    fn rest(w: &mut World, d: &Decision) {
        for spec in d.bundles.iter().flat_map(|b| &b.legs) {
            w.intents.push(resting(spec));
        }
    }

    fn resting(spec: &OrderSpec) -> IntentView {
        IntentView {
            intent_id: spec.intent_id.clone(),
            strategy: spec.strategy.clone(),
            exchange_id: spec.exchange_id.clone(),
            side: spec.side,
            action: spec.action,
            price: spec.price,
            remaining: spec.quantity.get(),
            created_at_ns: (NOW_S - 5) * 1_000_000_000,
            needle_derived: false,
        }
    }

    fn summary(d: &Decision) -> ScanSummary {
        let e = d
            .events
            .iter()
            .find(|e| e.kind == SCAN_EVENT)
            .expect("scan summary");
        serde_json::from_value(e.body.clone()).expect("summary json")
    }

    fn legs(d: &Decision) -> Vec<&OrderSpec> {
        d.bundles.iter().flat_map(|b| &b.legs).collect()
    }

    fn cost(d: &Decision) -> f64 {
        legs(d).iter().map(|l| l.max_notional()).sum()
    }

    #[test]
    fn rests_a_no_ladder_on_a_near_zero_market_once() {
        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        let orders = legs(&d);
        assert_eq!(orders.len(), 4, "{:?}", d.notes);
        let mut prices: Vec<u32> = orders.iter().map(|o| o.price.get()).collect();
        prices.sort_unstable();
        assert_eq!(prices, vec![100, 300, 500, 1000]);
        for o in &orders {
            assert_eq!((o.side, o.action), (Side::No, Action::Buy));
            assert_eq!(o.strategy, NAME);
            assert!(o.intent_id.starts_with("farquote:1:no:"), "{}", o.intent_id);
            assert_eq!(
                o.fair_yes, None,
                "no glitch-cap check: these sit through fair by design"
            );
            assert_eq!(o.ttl.as_secs(), 86_400);
            assert!(o.quantity.get() >= 1);
        }
        assert!(cost(&d) <= 0.15 * EQUITY + 1e-6, "within the reserve");
        assert!(cost(&d) <= 0.02 * EQUITY + 1e-6, "within the per-market cap");
        assert!(cost(&d) > 0.019 * EQUITY, "the whole per-market budget is used");
        let s = summary(&d);
        assert_eq!(
            (s.in_band, s.wanted, s.resting, s.placed, s.cancelled),
            (1, 4, 0, 4, 0)
        );
        assert_eq!(s.budget, 15_000.0);
        assert_eq!(s.equity, EQUITY);
        assert!((s.reserved - cost(&d)).abs() < 0.01);
        assert!(s.blocked.is_empty());

        // A second pass finds the ladder already resting and adds nothing.
        rest(&mut w, &d);
        let again = quoter().decide(&w.snap());
        assert!(again.is_empty(), "{:?}", again.notes);
        let s = summary(&again);
        assert_eq!((s.wanted, s.resting, s.placed), (4, 4, 0));
    }

    #[test]
    fn rests_a_yes_ladder_on_a_near_one_market() {
        let mut w = World::new(EQUITY);
        w.rule("1313", "m1", "", "", true);
        w.book("1313", &[(9500, 50)], &[(9700, 50)]);
        w.fair("1313", 0.96, None);
        let d = quoter().decide(&w.snap());
        let orders = legs(&d);
        assert_eq!(orders.len(), 4);
        assert!(
            orders
                .iter()
                .all(|o| o.side == Side::Yes && o.action == Action::Buy)
        );
        assert!(orders.iter().all(|o| o.price.get() <= 1000));
    }

    #[test]
    fn the_band_edge_is_inclusive_on_whole_pips() {
        let q = quoter();
        assert_eq!(q.side_for(0.05), Some(Side::No));
        assert_eq!(q.side_for(0.0501), None);
        assert_eq!(q.side_for(0.95), Some(Side::Yes));
        assert_eq!(q.side_for(0.9499), None);
        assert_eq!(q.side_for(0.5), None);
    }

    #[test]
    fn pulls_the_ladder_when_fair_leaves_the_band() {
        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        rest(&mut w, &d);
        w.fair("1", 0.10, None); // out of the band, still consistent with the 0.02 Cup price
        let d = quoter().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 4);
        assert!(d.cancels.iter().all(|c| c.reason == "fair left the band"));
        let s = summary(&d);
        assert_eq!((s.in_band, s.cancelled), (0, 4));

        // A fair the Cup price contradicts is a suspected wrong mapping: the ladder goes too.
        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        rest(&mut w, &d);
        w.fair("1", 0.50, None);
        let d = quoter().decide(&w.snap());
        assert_eq!(d.cancels.len(), 4);
        assert_eq!(summary(&d).blocked.get("mapping_error"), Some(&1));
    }

    #[test]
    fn leaves_ordinary_markets_alone() {
        let mut w = World::new(EQUITY);
        w.rule("1309", "m1", "", "", true);
        w.book("1309", &[(4500, 50)], &[(5500, 50)]);
        w.fair("1309", 0.5, None);
        let d = quoter().decide(&w.snap());
        assert!(d.is_empty());
        assert_eq!(summary(&d).in_band, 0);
    }

    #[test]
    fn a_stale_or_missing_fair_leaves_resting_ladders_alone() {
        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        rest(&mut w, &d);
        let before = w.intents.len();
        assert_eq!(before, 4);

        w.fair_at("1", 0.02, None, w.now_ns - 61 * 1_000_000_000);
        let d = quoter().decide(&w.snap());
        assert!(d.is_empty(), "stale: {:?}", d.cancels);
        assert_eq!(summary(&d).blocked.get("stale"), Some(&1));

        w.fair.clear();
        let d = quoter().decide(&w.snap());
        assert!(d.is_empty(), "no fair: {:?}", d.cancels);
        assert_eq!(summary(&d).blocked.get("no_fair"), Some(&1));

        w.fair("1", 0.02, None);
        w.books = susq_book::BookStore::new();
        let d = quoter().decide(&w.snap());
        assert!(d.is_empty(), "no Cup price: {:?}", d.cancels);
        assert_eq!(summary(&d).blocked.get("no_cup_price"), Some(&1));
        assert_eq!(w.intents.len(), before);
    }

    #[test]
    fn ladders_are_recognised_after_a_restart() {
        // A fresh process: the same levels rest under older intent ids read back from the log.
        let mut w = near_zero("1");
        for (price, qty) in [(100, 50_000), (300, 16_666), (500, 10_000), (1000, 5_000)] {
            let mut i = intent(
                &format!("farquote:1:no:{price}:1791400000"),
                NAME,
                "1",
                Side::No,
                Action::Buy,
                price,
            );
            i.remaining = qty;
            w.intents.push(i);
        }
        let d = quoter().decide(&w.snap());
        assert!(d.is_empty(), "{:?} {:?}", d.notes, d.cancels);
        let s = summary(&d);
        assert_eq!((s.wanted, s.resting, s.placed, s.cancelled), (4, 4, 0, 0));
        assert!((s.reserved - 2_000.0).abs() < 1.0, "{}", s.reserved);
    }

    #[test]
    fn duplicate_ladders_are_trimmed_to_one_per_level() {
        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        rest(&mut w, &d);
        // Simulate a ladder placed twice (what restarts without strategy attribution did).
        let dup: Vec<IntentView> = w
            .intents
            .iter()
            .map(|i| IntentView {
                intent_id: format!("{}:dup", i.intent_id),
                ..i.clone()
            })
            .collect();
        w.intents.extend(dup);
        assert_eq!(w.intents.len(), 8);
        let d = quoter().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 4);
        assert!(d.cancels.iter().all(|c| c.reason == "duplicate level"));
        let kept: HashSet<&str> = w
            .intents
            .iter()
            .map(|i| i.intent_id.as_str())
            .filter(|id| !d.cancels.iter().any(|c| &c.intent_id == id))
            .collect();
        assert_eq!(kept.len(), 4, "one rung per level survives");
    }

    #[test]
    fn oversized_rungs_are_replaced_at_todays_size() {
        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        rest(&mut w, &d);
        w.intents[0].remaining *= 2;
        let d = quoter().decide(&w.snap());
        assert_eq!(d.cancels.len(), 1);
        assert_eq!(d.cancels[0].reason, "oversized for today's budget");
        let orders = legs(&d);
        assert_eq!(orders.len(), 1, "re-placed at the new size");
        assert_eq!(orders[0].price, w.intents[0].price);
        let s = summary(&d);
        assert_eq!((s.resting, s.placed, s.cancelled), (3, 1, 1));
    }

    #[test]
    fn the_reserve_is_spread_evenly_over_markets_in_the_band() {
        let mut w = World::new(EQUITY);
        for i in 0..10 {
            let ex = format!("{i}");
            w.rule(&ex, &format!("m{i}"), "", "", true);
            w.book(&ex, &[(100, 50)], &[(300, 50)]);
            w.fair(&ex, 0.02, None);
        }
        let d = quoter().decide(&w.snap());
        assert_eq!(legs(&d).len(), 40);
        // 15 % of equity over ten markets: 1 500 each, below the 2 000 per-market cap.
        let per_market: f64 = legs(&d)
            .iter()
            .filter(|l| l.exchange_id.as_str() == "3")
            .map(|l| l.max_notional())
            .sum();
        assert!(
            per_market <= 1_500.0 + 1e-6 && per_market > 1_490.0,
            "{per_market}"
        );
        assert!(cost(&d) <= 0.15 * EQUITY + 1e-6);
        let s = summary(&d);
        assert_eq!((s.in_band, s.placed), (10, 40));
        assert!(s.blocked.is_empty());
        assert_eq!(
            quoter().wanted(Side::No, EQUITY, 1)[0].qty,
            50_000,
            "alone in the band a market gets the full 2 000"
        );
    }

    #[test]
    fn the_reserve_budget_holds_back_new_rungs() {
        // Another market's ladder already ties up almost the whole reserve.
        let mut w = near_zero("1");
        w.rule("2", "m2", "", "", true);
        let mut big = intent("farquote:2:no:100:1", NAME, "2", Side::No, Action::Buy, 100);
        big.remaining = 1_480_000; // 14 800 at 0.01
        w.intents.push(big);
        let d = quoter().decide(&w.snap());
        let orders = legs(&d);
        // 200 of reserve left: the 0.01 rung (500) and 0.03 rung (~500) do not fit; nothing does.
        assert!(orders.is_empty(), "{orders:?}");
        let s = summary(&d);
        assert_eq!(s.blocked.get("reserve_budget"), Some(&4));
        assert_eq!(s.in_band, 1, "market 2 has no fair, so it is not in the band");
        assert_eq!(s.blocked.get("no_fair"), Some(&1));
        assert_eq!(d.cancels.len(), 0, "market 2's ladder is kept: no verdict on it");
    }

    #[test]
    fn unverified_or_unmapped_markets_lose_their_ladders() {
        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        rest(&mut w, &d);
        w.rule("1", "m1", "", "", false);
        let d = quoter().decide(&w.snap());
        assert_eq!(d.cancels.len(), 4);
        assert!(d.cancels.iter().all(|c| c.reason == "unverified"));

        let mut w = near_zero("1");
        let d = quoter().decide(&w.snap());
        rest(&mut w, &d);
        let mut gone = World::new(EQUITY);
        gone.intents.clone_from(&w.intents);
        let d = quoter().decide(&gone.snap());
        assert_eq!(d.cancels.len(), 4);
        assert!(d.cancels.iter().all(|c| c.reason == "no longer mapped"));
    }

    #[test]
    fn disabled_does_nothing_and_a_broke_account_quotes_nothing() {
        let w = near_zero("1");
        let off = FarQuoter::new(FarQuoteCfg::default());
        let d = off.decide(&w.snap());
        assert!(d.is_empty() && d.events.is_empty());

        let mut w = near_zero("1");
        w.equity = 0.0;
        let d = quoter().decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(summary(&d).wanted, 0);
    }
}
