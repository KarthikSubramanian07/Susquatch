//! Election-night executor: trade race markets on needle posteriors, and roll appreciated legs.
//!
//! Acts only when enabled **and** the needle watchdog reports healthy. With the needle missing or
//! unhealthy it cancels every live needle-derived intent (from any strategy) and sends nothing.
//!
//! The results pollers' health gates races individually (`NeedleView::disputed_races`,
//! `NeedleView::stale_feed_races`, filled by rust-live from `EnrHealth` with `--election`): a
//! **disputed** race (sources disagree beyond tolerance) is refused outright and this strategy's
//! resting orders on it are cancelled; a race whose **feed is stale** gets no new buys, though a
//! held leg is still rolled (that reduces exposure on a posterior the needle still stands by).
//!
//! For each posterior, every verified market rule on that race maps to a win probability `q` by party
//! (`DEM` → `p_dem`, `REP` → `p_rep`; other rules are skipped). Then, per exchange, at most one action:
//!
//! * **Roll**: holding a side whose sell touch is within `roll_within_pts` of that side's `q`, sell the
//!   held side (a covered `Action::Sell`, qty ≤ holding) at no less than `q − roll`. Locked legs are
//!   never rolled.
//! * **Buy**: buy YES when `q − ask ≥ threshold`, NO when `(1 − q) − no_ask ≥ threshold`, where
//!   `threshold = max(min_edge_pts, se_mult × SE)`. With `require_corroboration`, a buy also needs
//!   `n_batches ≥ 2` or a same-direction move of the fresh real-money fair
//!   (`sign(p_yes − prev_p_yes) = sign(q − mid)`). Limit = `floor(side q − threshold)`; size =
//!   `floor(max_order_frac × equity / limit)`, capped by depth within the limit. A buy that would
//!   reduce a locked holding (buying its opposite side) is skipped.
//!
//! Model SE (heuristic). `RacePosterior` carries no standard error for the win probability, so we use
//! the binomial-style proxy [`model_se`] `= sqrt(q (1 − q) / (1 + n_batches))`: widest at q = 0.5 with
//! no reported batches, shrinking as county batches accumulate and as q approaches 0 or 1. It is a
//! deliberately conservative placeholder to be replaced by a needle-published SE.
//!
//! Every order is `needle_derived` with `fair_yes = q`.

use susq_book::L2Book;
use susq_contracts::v1::RacePosterior;
use susq_core::{Action, ExchangeId, OrderBundle, Pips, Side, config::EnCfg};

use crate::{
    Strategy,
    draft::{Draft, shares_for_budget},
    snapshot::{Decision, Snapshot},
};

/// Strategy name (intent id prefix).
pub const NAME: &str = "en";

/// Heuristic standard error of a posterior win probability: `sqrt(q (1 − q) / (1 + n_batches))`.
pub fn model_se(q: f64, n_batches: u32) -> f64 {
    let q = q.clamp(0.0, 1.0);
    (q * (1.0 - q) / (1.0 + f64::from(n_batches))).sqrt()
}

/// The election-night executor.
#[derive(Debug, Clone, PartialEq)]
pub struct EnExecutor {
    pub cfg: EnCfg,
}

/// Probability that `side` of a YES-probability-`q` contract pays.
fn side_prob(side: Side, q: f64) -> f64 {
    match side {
        Side::Yes => q,
        Side::No => 1.0 - q,
    }
}

impl EnExecutor {
    /// Election-night executor with its config.
    pub fn new(cfg: EnCfg) -> Self {
        Self { cfg }
    }

    /// Edge a buy must clear: `max(min_edge_pts / 100, se_mult × SE)`.
    pub fn threshold(&self, q: f64, n_batches: u32) -> f64 {
        (self.cfg.min_edge_pts / 100.0).max(self.cfg.se_mult * model_se(q, n_batches))
    }

    fn act(
        &self,
        snap: &Snapshot<'_>,
        post: &RacePosterior,
        ex: &ExchangeId,
        feed_stale: bool,
        d: &mut Decision,
    ) {
        let Some(rule) = snap.markets.rule(ex).filter(|r| r.verified) else {
            return;
        };
        let q = if rule.party.eq_ignore_ascii_case("DEM") {
            post.p_dem
        } else if rule.party.eq_ignore_ascii_case("REP") {
            post.p_rep
        } else {
            return;
        };
        if !(q.is_finite() && (0.0..=1.0).contains(&q)) {
            d.note(format!("en {ex}: posterior q {q} out of range"));
            return;
        }
        if snap.has_live_intent(NAME, ex) {
            return;
        }
        let Some(book) = snap.fresh_book(ex) else {
            d.note(format!("en {ex}: no fresh book"));
            return;
        };
        let held = snap.position(ex);
        let held_side = match held.signum() {
            1 => Some(Side::Yes),
            -1 => Some(Side::No),
            _ => None,
        };
        let locked = snap.is_locked(ex);
        if let Some(side) = held_side {
            if self.roll(snap, ex, book, side, held.abs(), q, locked, d) {
                return;
            }
        }
        if feed_stale {
            d.note(format!("en {ex}: results feed stale, no new buys"));
            return;
        }
        self.buy(snap, post, ex, book, q, held_side.filter(|_| locked), d);
    }

    /// Sell a held side whose bid is within `roll_within_pts` of its probability. True if acted on.
    #[allow(clippy::too_many_arguments)]
    fn roll(
        &self,
        snap: &Snapshot<'_>,
        ex: &ExchangeId,
        book: &L2Book,
        side: Side,
        held: i64,
        q: f64,
        locked: bool,
        d: &mut Decision,
    ) -> bool {
        let Some(floor) = Pips::ceil_from_prob(side_prob(side, q) - self.cfg.roll_within_pts / 100.0) else {
            return false;
        };
        if !book.touch(side, Action::Sell).is_some_and(|bid| bid >= floor) {
            return false;
        }
        if locked {
            d.note(format!("en {ex}: roll due but the leg is locked, not rolled"));
            return false;
        }
        let qty = book.sweep(side, Action::Sell, held, floor).qty;
        let draft = Draft {
            exchange: ex.clone(),
            side,
            action: Action::Sell,
            qty,
            price: floor,
            fair_yes: Some(q),
        };
        let key = format!("{ex}:roll");
        match draft.into_spec(snap, NAME, &key, self.cfg.order_ttl_s, true) {
            Some(spec) => {
                d.note(format!("en {ex}: roll {side} x{qty} @ ≥ {floor} (q {q:.3})"));
                d.bundles.push(OrderBundle::single(spec));
                true
            }
            None => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn buy(
        &self,
        snap: &Snapshot<'_>,
        post: &RacePosterior,
        ex: &ExchangeId,
        book: &L2Book,
        q: f64,
        locked_side: Option<Side>,
        d: &mut Decision,
    ) {
        let threshold = self.threshold(q, post.n_batches);
        for side in [Side::Yes, Side::No] {
            let p = side_prob(side, q);
            let Some(ask) = book.touch(side, Action::Buy) else {
                continue;
            };
            if p - ask.prob() + 1e-9 < threshold {
                continue;
            }
            if locked_side == Some(side.opposite()) {
                d.note(format!(
                    "en {ex}: buying {side} would reduce a locked leg, skipped"
                ));
                return;
            }
            if self.cfg.require_corroboration && !corroborated(snap, post, ex, book, q) {
                d.note(format!(
                    "en {ex}: {side} edge uncorroborated (n_batches {})",
                    post.n_batches
                ));
                return;
            }
            let Some(limit) = Pips::floor_from_prob(p - threshold) else {
                return;
            };
            let cap = shares_for_budget(self.cfg.max_order_frac * snap.equity, limit);
            let draft = Draft {
                exchange: ex.clone(),
                side,
                action: Action::Buy,
                qty: book.sweep(side, Action::Buy, cap, limit).qty,
                price: limit,
                fair_yes: Some(q),
            };
            let key = format!("{ex}:{side}");
            if let Some(spec) = draft.into_spec(snap, NAME, &key, self.cfg.order_ttl_s, true) {
                d.note(format!(
                    "en {ex}: buy {side} x{} @ {limit} (q {q:.3}, threshold {threshold:.3})",
                    spec.quantity
                ));
                d.bundles.push(OrderBundle::single(spec));
            }
            return;
        }
    }
}

/// At least two county batches, or the fresh real-money fair moved the same way the needle points.
fn corroborated(snap: &Snapshot<'_>, post: &RacePosterior, ex: &ExchangeId, book: &L2Book, q: f64) -> bool {
    if post.n_batches >= 2 {
        return true;
    }
    let (Some(mid), Some(moved)) = (book.mid(), snap.fresh_fair(ex).and_then(|f| f.moved())) else {
        return false;
    };
    let dir = q - mid;
    dir != 0.0 && moved != 0.0 && dir.signum() == moved.signum()
}

impl Strategy for EnExecutor {
    fn name(&self) -> &'static str {
        NAME
    }

    fn decide(&self, snap: &Snapshot<'_>) -> Decision {
        let mut d = Decision::default();
        if !self.cfg.enabled {
            return d;
        }
        let needle = match snap.needle {
            Some(n) if n.healthy => n,
            _ => {
                for i in snap.intents.iter().filter(|i| i.needle_derived) {
                    d.cancel(&i.intent_id, "needle unhealthy");
                }
                d.note("en: needle missing or unhealthy, standing down");
                return d;
            }
        };
        let mut races: Vec<&RacePosterior> = needle.posteriors.values().collect();
        races.sort_by(|a, b| a.race_id.cmp(&b.race_id));
        for post in races {
            let exchanges = snap.markets.exchanges_for_race(&post.race_id);
            if needle.disputed_races.contains(&post.race_id) {
                for i in snap
                    .live_intents(NAME)
                    .filter(|i| exchanges.contains(&i.exchange_id))
                {
                    d.cancel(&i.intent_id, "race disputed");
                }
                d.note(format!(
                    "en {}: results sources disagree, race disputed, not acting",
                    post.race_id
                ));
                continue;
            }
            let feed_stale = needle.stale_feed_races.contains(&post.race_id);
            for ex in exchanges {
                self.act(snap, post, ex, feed_stale, &mut d);
            }
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{World, intent};

    const RACE: &str = "2026-SEN-OH";

    fn cfg() -> EnCfg {
        EnCfg {
            enabled: true,
            ..EnCfg::default()
        }
    }

    /// Exchange 1 = DEM wins, exchange 2 = REP wins, both on [`RACE`].
    fn world(p_dem: f64, n_batches: u32) -> World {
        let mut w = World::new(100_000.0);
        w.rule("1", "m1", RACE, "DEM", true);
        w.rule("2", "m1", RACE, "REP", true);
        w.needle(true, &[(RACE, p_dem, 1.0 - p_dem, n_batches)]);
        w
    }

    fn legs(d: &Decision) -> Vec<(String, Side, Action, u32, i64)> {
        d.bundles
            .iter()
            .flat_map(|b| &b.legs)
            .map(|l| {
                (
                    l.exchange_id.to_string(),
                    l.side,
                    l.action,
                    l.price.get(),
                    l.quantity.get(),
                )
            })
            .collect()
    }

    #[test]
    fn disabled_does_nothing() {
        let mut w = world(0.9, 50);
        w.book("1", &[(5000, 100)], &[(5500, 100)]);
        assert_eq!(
            EnExecutor::new(EnCfg::default()).decide(&w.snap()),
            Decision::default()
        );
    }

    #[test]
    fn unhealthy_needle_cancels_needle_derived_intents_only() {
        let mut w = world(0.9, 50);
        w.book("1", &[(5000, 100)], &[(5500, 100)]);
        let mut en = intent("en:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5000);
        en.needle_derived = true;
        let mut other = intent("scenario:9:1", "scenario", "9", Side::Yes, Action::Buy, 5000);
        other.needle_derived = true;
        w.intents.extend([
            en,
            other,
            intent("sniper:1:yes:1", "sniper", "1", Side::Yes, Action::Buy, 4000),
        ]);
        w.needle.as_mut().expect("needle").healthy = false;
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        let ids: Vec<&str> = d.cancels.iter().map(|c| c.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["en:1:yes:1", "scenario:9:1"]);
        assert!(d.cancels.iter().all(|c| c.reason == "needle unhealthy"));

        w.needle = None;
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert_eq!(d.cancels.len(), 2);
    }

    #[test]
    fn model_se_heuristic() {
        assert!((model_se(0.5, 0) - 0.5).abs() < 1e-12);
        assert!((model_se(0.7, 99) - (0.21f64 / 100.0).sqrt()).abs() < 1e-12);
        assert_eq!(model_se(1.0, 3), 0.0);
    }

    #[test]
    fn buy_threshold_is_the_larger_of_min_edge_and_three_se() {
        // q = 0.70 with 99 batches: SE 0.0458, 3×SE = 0.137 > 0.08.
        let en = EnExecutor::new(cfg());
        assert!((en.threshold(0.70, 99) - 3.0 * (0.0021f64).sqrt()).abs() < 1e-12);
        let mut w = world(0.70, 99);
        w.book("1", &[(5000, 100)], &[(5700, 100)]); // edge 0.13: above 8 pts, below 3 SE
        assert!(legs(&en.decide(&w.snap())).is_empty());
        w.book("1", &[(5000, 100)], &[(5500, 100)]); // edge 0.15 ≥ 0.137
        // Limit floor(0.70 − 0.1375) = 0.56; the 0.55 ask has 100 shares.
        assert_eq!(
            legs(&en.decide(&w.snap())),
            vec![("1".into(), Side::Yes, Action::Buy, 5600, 100)]
        );

        // Plenty of batches: SE is small, the 8-point floor binds.
        let mut w = world(0.70, 9_999);
        w.book("1", &[(5000, 100)], &[(6150, 100)]); // edge 0.085 ≥ 0.08
        let d = en.decide(&w.snap());
        let o = &d.bundles[0].legs[0];
        assert!(o.needle_derived);
        assert_eq!(o.fair_yes, Some(0.70));
        assert_eq!((o.price.get(), o.quantity.get()), (6200, 100));
        assert!(o.intent_id.starts_with("en:1:yes:"));
    }

    #[test]
    fn buys_no_on_the_other_party_and_sizes_by_order_fraction() {
        let mut w = world(0.90, 9_999); // REP wins with 0.10 → NO on exchange 2 is worth 0.90.
        w.equity = 10_000.0; // 2% = 200 SUSQies at the 0.82 NO limit (0.90 − 8 pts) → 243 shares.
        // YES bid 0.25 → NO ask 0.75; edge 0.15.
        w.book("2", &[(2500, 1_000)], &[(3000, 1_000)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("2".into(), Side::No, Action::Buy, 8200, 243)]
        );
    }

    #[test]
    fn corroboration_needs_two_batches_or_a_same_direction_fair_move() {
        let en = EnExecutor::new(EnCfg {
            se_mult: 0.0,
            ..cfg()
        });
        let mut w = world(0.80, 1);
        w.book("1", &[(6000, 100)], &[(6500, 100)]); // mid 0.625, q above it
        let d = en.decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("uncorroborated")));

        w.fair("1", 0.66, Some(0.62)); // real money moved up too
        assert_eq!(en.decide(&w.snap()).bundles.len(), 1);

        w.fair("1", 0.60, Some(0.62)); // real money moved the other way
        assert!(en.decide(&w.snap()).bundles.is_empty());

        let mut w = world(0.80, 2);
        w.book("1", &[(6000, 100)], &[(6500, 100)]);
        assert_eq!(en.decide(&w.snap()).bundles.len(), 1);
    }

    #[test]
    fn rolls_an_appreciated_leg_near_q() {
        let mut w = world(0.70, 9_999);
        w.position("1", 40);
        // Bid 0.69 ≥ 0.70 − 0.02 → sell the 40 YES at no less than 0.68.
        w.book("1", &[(6900, 100)], &[(7100, 100)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("1".into(), Side::Yes, Action::Sell, 6800, 40)]
        );
        // Holding NO on exchange 2 (REP, q 0.30 → NO worth 0.70): NO bid = 1 − 0.31 = 0.69.
        let mut w = world(0.70, 9_999);
        w.position("2", -15);
        w.book("2", &[(2900, 100)], &[(3100, 100)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("2".into(), Side::No, Action::Sell, 6800, 15)]
        );
    }

    #[test]
    fn locked_legs_are_not_rolled_or_reduced() {
        let mut w = world(0.70, 9_999);
        w.position("1", 40);
        w.target("1", 40, 0.9, true);
        w.book("1", &[(6900, 100)], &[(7100, 100)]);
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("locked")));

        // Bid far above q would be a NO buy, which would reduce the locked YES holding.
        let mut w = world(0.50, 9_999);
        w.position("1", 40);
        w.target("1", 40, 0.9, true);
        w.book("1", &[(6000, 100)], &[(6100, 100)]);
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", legs(&d));
    }

    /// Gap 8 of the readiness audit: "the election-night executor must refuse to act on disputed
    /// races" (docs/ELECTION_NIGHT.md §4), including what it already has resting on them.
    #[test]
    fn disputed_races_are_refused_and_their_orders_cancelled() {
        let mut w = world(0.90, 9_999);
        w.book("1", &[(5000, 100)], &[(5500, 100)]); // a 35 pt edge
        w.rule("9", "m9", "2026-GOV-OH", "DEM", true);
        let mut on_race = intent("en:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5000);
        on_race.needle_derived = true;
        let mut elsewhere = intent("en:9:yes:1", NAME, "9", Side::Yes, Action::Buy, 5000);
        elsewhere.needle_derived = true;
        w.intents.extend([on_race, elsewhere]);
        w.dispute(RACE);
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", legs(&d));
        let ids: Vec<&str> = d.cancels.iter().map(|c| c.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["en:1:yes:1"], "only the disputed race's orders go");
        assert_eq!(d.cancels[0].reason, "race disputed");
        assert!(d.notes.iter().any(|n| n.contains("disputed")));
        // The dispute clears: trading resumes (the worked exchange still has its live intent).
        w.needle.as_mut().expect("needle").disputed_races.clear();
        w.intents.clear();
        assert_eq!(EnExecutor::new(cfg()).decide(&w.snap()).bundles.len(), 1);
    }

    #[test]
    fn stale_feed_skips_buys_but_still_rolls() {
        let mut w = world(0.70, 9_999);
        w.stale_feed(RACE);
        w.book("1", &[(5000, 100)], &[(6150, 100)]); // edge 0.085 ≥ 8 pts: would buy
        let d = EnExecutor::new(cfg()).decide(&w.snap());
        assert!(d.bundles.is_empty(), "{:?}", legs(&d));
        assert!(d.cancels.is_empty());
        assert!(d.notes.iter().any(|n| n.contains("feed stale")));
        // A held leg whose bid is near q is still rolled: that only reduces exposure.
        w.position("1", 40);
        w.book("1", &[(6900, 100)], &[(7100, 100)]);
        assert_eq!(
            legs(&EnExecutor::new(cfg()).decide(&w.snap())),
            vec![("1".into(), Side::Yes, Action::Sell, 6800, 40)]
        );
        // Once the feed is back, the buy goes through.
        w.position("1", 0);
        w.book("1", &[(5000, 100)], &[(6150, 100)]);
        w.needle.as_mut().expect("needle").stale_feed_races.clear();
        assert_eq!(EnExecutor::new(cfg()).decide(&w.snap()).bundles.len(), 1);
    }

    #[test]
    fn skips_worked_exchanges_and_other_parties() {
        let mut w = world(0.90, 9_999);
        w.rule("3", "m1", RACE, "IND", true);
        w.book("1", &[(5000, 100)], &[(5500, 100)]);
        w.book("3", &[(5000, 100)], &[(5500, 100)]);
        let mut live = intent("en:1:yes:1", NAME, "1", Side::Yes, Action::Buy, 5000);
        live.needle_derived = true;
        w.intents.push(live);
        assert!(EnExecutor::new(cfg()).decide(&w.snap()).bundles.is_empty());
    }
}
