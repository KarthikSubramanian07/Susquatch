//! Property: no accepted bundle ever leaves a cap breached.
//!
//! The expected exposures are recomputed here independently of the implementation: a position is
//! |net shares| × price of the held side at the mark; every live order is its remainder at its
//! limit; every leg adds the shares it opens (closing shares are free) at its limit.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use proptest::prelude::*;
use susq_contracts::v1::{MarketMap, MarketRule};
use susq_core::{
    Action, BookSide, Calendar, ExchangeId, MarketIndex, OrderBundle, OrderSpec, Pips, Qty, Side,
    TournamentId,
};
use susq_risk::{LiveOrder, PreTrade, RiskContext, RiskLimits};

const EXCHANGES: [(&str, &str, &[&str]); 5] = [
    ("1", "m1", &["national", "state:OH"]),
    ("2", "m1", &["national", "state:OH"]),
    ("3", "m3", &["national", "state:ME"]),
    ("4", "m4", &["house"]),
    ("5", "m5", &[]),
];

fn markets() -> MarketIndex {
    MarketIndex::new(&MarketMap {
        header: None,
        markets: EXCHANGES
            .iter()
            .map(|(ex, m, c)| MarketRule {
                exchange_id: (*ex).into(),
                market_id: (*m).into(),
                clusters: c.iter().map(|s| (*s).to_owned()).collect(),
                verified: true,
                ..Default::default()
            })
            .collect(),
    })
}

fn limits() -> RiskLimits {
    RiskLimits {
        per_market_gross_frac: 0.05,
        total_gross_frac: 0.60,
        cluster_kind_frac: [("national", 0.25), ("state", 0.10), ("house", 0.20)]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
        cluster_override_frac: [("state:ME".to_owned(), 0.04)].into(),
        max_order_notional_frac: 0.02,
        glitch_through_fair: 0.15,
        glitch_max_frac: 0.02,
        needle_glitch_through_fair: 0.40,
    }
}

fn ex() -> impl Strategy<Value = ExchangeId> {
    (0usize..EXCHANGES.len()).prop_map(|i| ExchangeId::new(EXCHANGES[i].0))
}

fn price() -> impl Strategy<Value = Pips> {
    (1u32..=199).prop_map(|t| Pips::new(t * 50).expect("valid"))
}

fn leg() -> impl Strategy<Value = OrderSpec> {
    (
        ex(),
        prop_oneof![Just(Side::Yes), Just(Side::No)],
        prop_oneof![Just(Action::Buy), Just(Action::Sell)],
        1i64..600,
        price(),
    )
        .prop_map(|(exchange_id, side, action, q, price)| OrderSpec {
            intent_id: "p".into(),
            strategy: "p".into(),
            exchange_id,
            market_id: None,
            side,
            action,
            quantity: Qty(q),
            price,
            ttl: Duration::from_secs(30),
            needle_derived: false,
            fair_yes: None,
        })
}

fn live() -> impl Strategy<Value = LiveOrder> {
    (
        ex(),
        prop_oneof![Just(BookSide::Bid), Just(BookSide::Ask)],
        price(),
        1i64..300,
    )
        .prop_map(|(exchange_id, book_side, yes_price, remaining)| LiveOrder {
            exchange_id,
            book_side,
            yes_price,
            remaining,
        })
}

fn exposure(net: i64, yes_mark: f64) -> f64 {
    let px = if net >= 0 { yes_mark } else { 1.0 - yes_mark };
    net.unsigned_abs() as f64 * px
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, .. ProptestConfig::default() })]

    #[test]
    fn accepted_bundles_respect_every_cap(
        positions in proptest::collection::hash_map(ex(), -3000i64..3000, 0..5),
        marks in proptest::collection::hash_map(ex(), 0.01f64..0.99, 0..5),
        live_orders in proptest::collection::vec(live(), 0..4),
        legs in proptest::collection::vec(leg(), 1..4),
        equity in 1_000.0f64..200_000.0,
    ) {
        let calendar = Calendar {
            open_utc: "2026-10-01T16:00:00Z".parse().expect("ts"),
            lock_utc: "2026-11-04T17:00:00Z".parse().expect("ts"),
            last_order_utc: "2026-11-04T16:55:00Z".parse().expect("ts"),
        };
        let tid = TournamentId::new("t");
        let markets = markets();
        let locked = HashSet::new();
        let ctx = RiskContext {
            now: "2026-10-10T12:00:00Z".parse().expect("ts"),
            equity,
            calendar: &calendar,
            tournament: &tid,
            positions: &positions,
            marks: &marks,
            live_orders: &live_orders,
            markets: &markets,
            locked: &locked,
            glitch_used: 0.0,
            cash: None,
            bundle_cost: None,
        };
        let pt = PreTrade { limits: limits() };
        let bundle = OrderBundle { legs: legs.clone(), atomic: true };
        if pt.check_bundle(&ctx, &bundle, &tid).is_err() {
            return Ok(());
        }

        // Independent recomputation.
        let mut leg_mark: HashMap<ExchangeId, f64> = HashMap::new();
        for l in &legs {
            prop_assert!(l.max_notional() <= 0.02 * equity + 1e-6, "order notional cap");
            leg_mark.entry(l.exchange_id.clone()).or_insert(l.yes_space().1.prob());
        }
        let mark = |e: &ExchangeId| marks.get(e).copied().or_else(|| leg_mark.get(e).copied()).unwrap_or(0.5);
        let mut after: HashMap<ExchangeId, f64> = positions
            .iter()
            .map(|(e, net)| (e.clone(), exposure(*net, mark(e))))
            .collect();
        for o in &live_orders {
            let px = match o.book_side {
                BookSide::Bid => o.yes_price.prob(),
                BookSide::Ask => 1.0 - o.yes_price.prob(),
            };
            *after.entry(o.exchange_id.clone()).or_default() += o.remaining as f64 * px;
        }
        let mut held = positions.clone();
        let mut increased: Vec<&ExchangeId> = Vec::new();
        for l in &legs {
            let delta = l.yes_delta();
            let pos = held.get(&l.exchange_id).copied().unwrap_or(0);
            let closing = if pos * delta < 0 { delta.abs().min(pos.abs()) } else { 0 };
            let (side, yes) = l.yes_space();
            let px = match side {
                BookSide::Bid => yes.prob(),
                BookSide::Ask => 1.0 - yes.prob(),
            };
            let opened = (delta.abs() - closing) as f64 * px;
            if opened > 1e-9 {
                increased.push(&l.exchange_id);
            }
            *after.entry(l.exchange_id.clone()).or_default() += opened;
            *held.entry(l.exchange_id.clone()).or_default() += delta;
        }
        if increased.is_empty() {
            return Ok(()); // pure risk reduction is always allowed
        }
        let exp_after = |e: &ExchangeId| after.get(e).copied().unwrap_or(0.0);
        let gross: f64 = after.values().sum();
        prop_assert!(gross <= 0.60 * equity + 1e-6, "gross {gross} > cap");
        for e in &increased {
            let m = markets.market_of(e).map(|m| m.to_string());
            let total: f64 = after.keys().filter(|x| markets.market_of(x).map(|m| m.to_string()) == m && (m.is_some() || x == e)).map(exp_after).sum();
            prop_assert!(total <= 0.05 * equity + 1e-6, "market cap breached on {e}: {total}");
            for c in markets.clusters(e) {
                let cap = limits().cluster_frac(c).expect("configured") * equity;
                let total: f64 = after.keys().filter(|x| markets.clusters(x).contains(c)).map(exp_after).sum();
                prop_assert!(total <= cap + 1e-6, "cluster {c} breached: {total} > {cap}");
            }
        }
    }
}
