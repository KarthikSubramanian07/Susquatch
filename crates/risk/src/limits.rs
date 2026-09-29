//! Pre-trade checks: caps, glitch cap, locked legs, time gate, tournament, self-cross.
//!
//! Exposure is measured as money at risk: `|net shares| × price of the held side` (YES at the YES
//! mark, NO at `1 − mark`), with every live order counted as if it filled at its limit. An order
//! that does not increase an exchange's exposure is never blocked by a cap (reducing risk is always
//! allowed, except on locked legs).
//!
//! **Buying power** (only when [`RiskContext::cash`] is known): a bundle must fit in cash minus what
//! our live orders could consume (each at full notional). A bundle consumes the full notional of the
//! shares it opens (closing shares are free) unless [`RiskContext::bundle_cost`] is set, which
//! rust-live does only with `[risk] use_collateral_buying_power = true`: then it consumes the modeled
//! ALL effective cost (see `susq_platform::collateral`).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
};

use chrono::{DateTime, Utc};
use susq_core::{
    BookSide, Calendar, ExchangeId, MarketIndex, OrderBundle, OrderSpec, Pips, TournamentId, config::RiskCfg,
    market_map::cluster_kind,
};

/// Why an order was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Rejection {
    #[error("trading frozen: {0}")]
    Frozen(String),
    #[error("time gate: outside the trading window")]
    TimeGate,
    #[error("tournament mismatch (expected {expected}, got {got})")]
    WrongTournament { expected: String, got: String },
    #[error("locked leg {0} may not be reduced")]
    LockedLeg(ExchangeId),
    #[error("per-market cap on {key}: {after:.2} > {cap:.2}")]
    MarketCap { key: String, after: f64, cap: f64 },
    #[error("cluster cap on {cluster}: {after:.2} > {cap:.2}")]
    ClusterCap { cluster: String, after: f64, cap: f64 },
    #[error("gross cap: {after:.2} > {cap:.2}")]
    GrossCap { after: f64, cap: f64 },
    #[error("order notional {notional:.2} > {cap:.2}")]
    OrderNotional { notional: f64, cap: f64 },
    #[error("glitch cap: {after:.2} > {cap:.2}")]
    GlitchCap { after: f64, cap: f64 },
    #[error("order would cross our own resting order on {0}")]
    SelfCross(ExchangeId),
    #[error("no equity to size against")]
    NoEquity,
    #[error("buying power: bundle needs {need:.2}, {available:.2} available")]
    BuyingPower { need: f64, available: f64 },
}

/// Cash a bundle would consume under a collateral model, given signed positions (+YES / −NO).
pub trait BundleCost: fmt::Debug {
    fn cash_needed(&self, positions: &HashMap<ExchangeId, i64>, bundle: &OrderBundle) -> f64;
}

/// Cap fractions (of equity), from `[risk]` plus optional per-cluster overrides from targets.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskLimits {
    pub per_market_gross_frac: f64,
    pub total_gross_frac: f64,
    /// Cluster kind (`national`, `region`, `state`, `house`) → fraction.
    pub cluster_kind_frac: BTreeMap<String, f64>,
    /// Full cluster label (`state:OH`) → fraction; tighter of this and the kind cap applies.
    pub cluster_override_frac: BTreeMap<String, f64>,
    pub max_order_notional_frac: f64,
    pub glitch_through_fair: f64,
    pub glitch_max_frac: f64,
}

impl RiskLimits {
    /// From `[risk]`.
    pub fn from_cfg(cfg: &RiskCfg) -> Self {
        Self {
            per_market_gross_frac: cfg.per_market_gross_frac,
            total_gross_frac: cfg.total_gross_frac,
            cluster_kind_frac: cfg.clusters.clone(),
            cluster_override_frac: BTreeMap::new(),
            max_order_notional_frac: cfg.max_order_notional_frac,
            glitch_through_fair: cfg.glitch_through_fair_pts / 100.0,
            glitch_max_frac: cfg.glitch_max_frac,
        }
    }

    /// Cap fraction for a cluster label (tightest applicable), if any.
    pub fn cluster_frac(&self, cluster: &str) -> Option<f64> {
        let kind = self.cluster_kind_frac.get(cluster_kind(cluster)).copied();
        let over = self.cluster_override_frac.get(cluster).copied();
        match (kind, over) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// One of our live orders (resting or in flight), in YES space.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveOrder {
    pub exchange_id: ExchangeId,
    pub book_side: BookSide,
    /// YES-space limit price.
    pub yes_price: Pips,
    /// Unfilled shares.
    pub remaining: i64,
}

/// Everything a pre-trade check needs. Built fresh by the live loop for each decision round.
#[derive(Debug, Clone)]
pub struct RiskContext<'a> {
    pub now: DateTime<Utc>,
    pub equity: f64,
    pub calendar: &'a Calendar,
    pub tournament: &'a TournamentId,
    /// Signed positions (+YES / −NO).
    pub positions: &'a HashMap<ExchangeId, i64>,
    /// YES marks per exchange (fallback: the order's own price).
    pub marks: &'a HashMap<ExchangeId, f64>,
    pub live_orders: &'a [LiveOrder],
    pub markets: &'a MarketIndex,
    /// Legs strategies may not reduce (from targets).
    pub locked: &'a HashSet<ExchangeId>,
    /// Glitch-trade notional already committed.
    pub glitch_used: f64,
    /// Tournament cash, when known (`None`: buying power is left to the platform).
    pub cash: Option<f64>,
    /// Collateral-aware bundle cost; `None` = full opening notional (the default, conservative).
    pub bundle_cost: Option<&'a dyn BundleCost>,
}

/// Signed position after every live order fills, per exchange.
fn projected_positions(ctx: &RiskContext<'_>) -> HashMap<ExchangeId, i64> {
    let mut out = ctx.positions.clone();
    for o in ctx.live_orders {
        *out.entry(o.exchange_id.clone()).or_default() += o.book_side.yes_delta_sign() * o.remaining;
    }
    out
}

/// Full notional of the shares a bundle opens (closing shares, which net against a position and
/// pay out, are free), legs applied in order.
pub fn opening_notional(positions: &HashMap<ExchangeId, i64>, legs: &[OrderSpec]) -> f64 {
    let mut held = positions.clone();
    let mut total = 0.0;
    for leg in legs {
        let delta = leg.yes_delta();
        let pos = held.get(&leg.exchange_id).copied().unwrap_or(0);
        let closing = if pos * delta < 0 {
            delta.abs().min(pos.abs())
        } else {
            0
        };
        let (side, yes) = leg.yes_space();
        let open_px = match side {
            BookSide::Bid => yes.prob(),
            BookSide::Ask => 1.0 - yes.prob(),
        };
        total += (delta.abs() - closing) as f64 * open_px;
        *held.entry(leg.exchange_id.clone()).or_default() += delta;
    }
    total
}

/// Money at risk of a signed position at a YES mark.
fn exposure(net: i64, yes_mark: f64) -> f64 {
    let px = if net >= 0 { yes_mark } else { 1.0 - yes_mark };
    net.unsigned_abs() as f64 * px.clamp(0.0, 1.0)
}

/// The pre-trade checker.
#[derive(Debug, Clone)]
pub struct PreTrade {
    pub limits: RiskLimits,
}

impl PreTrade {
    /// Check a whole bundle as if all its legs fill. `Ok` means every cap still holds afterwards.
    pub fn check_bundle(
        &self,
        ctx: &RiskContext<'_>,
        bundle: &OrderBundle,
        request_tournament: &TournamentId,
    ) -> Result<(), Rejection> {
        if !ctx.calendar.may_place(ctx.now) {
            return Err(Rejection::TimeGate);
        }
        if ctx.tournament.as_str().is_empty() || request_tournament != ctx.tournament {
            return Err(Rejection::WrongTournament {
                expected: ctx.tournament.to_string(),
                got: request_tournament.to_string(),
            });
        }
        if !(ctx.equity.is_finite() && ctx.equity > 0.0) {
            return Err(Rejection::NoEquity);
        }
        self.check_self_cross(ctx, &bundle.legs)?;

        let before = projected_positions(ctx);
        let mut after = before.clone();
        let mut order_marks: HashMap<ExchangeId, f64> = HashMap::new();
        for leg in &bundle.legs {
            let notional = leg.max_notional();
            let cap = self.limits.max_order_notional_frac * ctx.equity;
            if notional > cap + 1e-9 {
                return Err(Rejection::OrderNotional { notional, cap });
            }
            *after.entry(leg.exchange_id.clone()).or_default() += leg.yes_delta();
            order_marks
                .entry(leg.exchange_id.clone())
                .or_insert(leg.yes_space().1.prob());
        }
        let mark = |ex: &ExchangeId| {
            ctx.marks
                .get(ex)
                .copied()
                .or_else(|| order_marks.get(ex).copied())
                .unwrap_or(0.5)
        };

        let touched: HashSet<&ExchangeId> = bundle.legs.iter().map(|l| &l.exchange_id).collect();
        for ex in &touched {
            let b = before.get(*ex).copied().unwrap_or(0);
            let a = after.get(*ex).copied().unwrap_or(0);
            if ctx.locked.contains(*ex) && (a.abs() < b.abs() || a.signum() * b.signum() < 0) {
                return Err(Rejection::LockedLeg((*ex).clone()));
            }
        }
        let increases: Vec<&ExchangeId> = touched
            .iter()
            .copied()
            .filter(|ex| {
                let m = mark(ex);
                exposure(after.get(*ex).copied().unwrap_or(0), m)
                    > exposure(before.get(*ex).copied().unwrap_or(0), m) + 1e-9
            })
            .collect();
        self.check_buying_power(ctx, bundle)?;
        if increases.is_empty() {
            return Ok(());
        }
        self.check_glitch(ctx, &bundle.legs)?;
        self.check_caps(ctx, &after, &increases, &mark)
    }

    fn check_self_cross(&self, ctx: &RiskContext<'_>, legs: &[OrderSpec]) -> Result<(), Rejection> {
        let crosses =
            |side: BookSide, price: Pips, other_side: BookSide, other: Pips| match (side, other_side) {
                (BookSide::Bid, BookSide::Ask) => price >= other,
                (BookSide::Ask, BookSide::Bid) => price <= other,
                _ => false,
            };
        for (i, leg) in legs.iter().enumerate() {
            let (side, price) = leg.yes_space();
            let resting = ctx
                .live_orders
                .iter()
                .filter(|o| o.exchange_id == leg.exchange_id && o.remaining > 0)
                .map(|o| (o.book_side, o.yes_price));
            let siblings = legs
                .iter()
                .enumerate()
                .filter(|(j, l)| *j != i && l.exchange_id == leg.exchange_id)
                .map(|(_, l)| l.yes_space());
            if resting.chain(siblings).any(|(s, p)| crosses(side, price, s, p)) {
                return Err(Rejection::SelfCross(leg.exchange_id.clone()));
            }
        }
        Ok(())
    }

    fn check_buying_power(&self, ctx: &RiskContext<'_>, bundle: &OrderBundle) -> Result<(), Rejection> {
        let Some(cash) = ctx.cash else {
            return Ok(());
        };
        let reserved: f64 = ctx
            .live_orders
            .iter()
            .map(|o| {
                let px = match o.book_side {
                    BookSide::Bid => o.yes_price.prob(),
                    BookSide::Ask => 1.0 - o.yes_price.prob(),
                };
                o.remaining.max(0) as f64 * px
            })
            .sum();
        let need = match ctx.bundle_cost {
            Some(model) => model.cash_needed(ctx.positions, bundle),
            None => opening_notional(ctx.positions, &bundle.legs),
        };
        let available = cash - reserved;
        if need > 1e-9 && need > available + 1e-9 {
            return Err(Rejection::BuyingPower { need, available });
        }
        Ok(())
    }

    fn check_glitch(&self, ctx: &RiskContext<'_>, legs: &[OrderSpec]) -> Result<(), Rejection> {
        let glitch: f64 = legs
            .iter()
            .filter(|l| self.is_glitch(l))
            .map(OrderSpec::max_notional)
            .sum();
        if glitch > 0.0 {
            let cap = self.limits.glitch_max_frac * ctx.equity;
            let after = ctx.glitch_used + glitch;
            if after > cap + 1e-9 {
                return Err(Rejection::GlitchCap { after, cap });
            }
        }
        Ok(())
    }

    /// An order priced `glitch_through_fair` or more through fair in our favour.
    pub fn is_glitch(&self, leg: &OrderSpec) -> bool {
        let Some(fair_yes) = leg.fair_yes else {
            return false;
        };
        let (side, yes_price) = leg.yes_space();
        let through = match side {
            BookSide::Bid => fair_yes - yes_price.prob(),
            BookSide::Ask => yes_price.prob() - fair_yes,
        };
        through >= self.limits.glitch_through_fair - 1e-9
    }

    fn check_caps(
        &self,
        ctx: &RiskContext<'_>,
        after: &HashMap<ExchangeId, i64>,
        increases: &[&ExchangeId],
        mark: &dyn Fn(&ExchangeId) -> f64,
    ) -> Result<(), Rejection> {
        let eq = ctx.equity;
        let exp = |ex: &ExchangeId| exposure(after.get(ex).copied().unwrap_or(0), mark(ex));

        let gross: f64 = after.keys().map(exp).sum();
        let cap = self.limits.total_gross_frac * eq;
        if gross > cap + 1e-9 {
            return Err(Rejection::GrossCap { after: gross, cap });
        }

        let market_key = |ex: &ExchangeId| {
            ctx.markets
                .market_of(ex)
                .map_or_else(|| format!("exchange:{ex}"), |m| format!("market:{m}"))
        };
        let cap = self.limits.per_market_gross_frac * eq;
        for ex in increases {
            let key = market_key(ex);
            let total: f64 = after.keys().filter(|e| market_key(e) == key).map(exp).sum();
            if total > cap + 1e-9 {
                return Err(Rejection::MarketCap {
                    key,
                    after: total,
                    cap,
                });
            }
        }

        let clusters: HashSet<&String> = increases.iter().flat_map(|ex| ctx.markets.clusters(ex)).collect();
        for cluster in clusters {
            let Some(frac) = self.limits.cluster_frac(cluster) else {
                continue;
            };
            let total: f64 = after
                .keys()
                .filter(|e| ctx.markets.clusters(e).iter().any(|c| c == cluster))
                .map(exp)
                .sum();
            let cap = frac * eq;
            if total > cap + 1e-9 {
                return Err(Rejection::ClusterCap {
                    cluster: cluster.clone(),
                    after: total,
                    cap,
                });
            }
        }
        Ok(())
    }

    /// Current cluster utilisation (exposure / cap) for every cluster in use, for MM skew and status.
    pub fn cluster_utilization(&self, ctx: &RiskContext<'_>) -> HashMap<String, f64> {
        let now = projected_positions(ctx);
        let mark = |ex: &ExchangeId| ctx.marks.get(ex).copied().unwrap_or(0.5);
        let mut totals: HashMap<String, f64> = HashMap::new();
        for (ex, q) in &now {
            for c in ctx.markets.clusters(ex) {
                *totals.entry(c.clone()).or_default() += exposure(*q, mark(ex));
            }
        }
        totals
            .into_iter()
            .filter_map(|(c, total)| {
                let cap = self.limits.cluster_frac(&c)? * ctx.equity;
                (cap > 0.0).then(|| (c, total / cap))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use susq_contracts::v1::{MarketMap, MarketRule};
    use susq_core::{Action, Qty, Side};

    pub(crate) fn limits() -> RiskLimits {
        RiskLimits {
            per_market_gross_frac: 0.05,
            total_gross_frac: 0.60,
            cluster_kind_frac: [("national".to_owned(), 0.25), ("state".to_owned(), 0.10)].into(),
            cluster_override_frac: BTreeMap::new(),
            max_order_notional_frac: 0.02,
            glitch_through_fair: 0.15,
            glitch_max_frac: 0.02,
        }
    }

    fn calendar() -> Calendar {
        Calendar {
            open_utc: "2026-10-01T16:00:00Z".parse().expect("ts"),
            lock_utc: "2026-11-04T17:00:00Z".parse().expect("ts"),
            last_order_utc: "2026-11-04T16:55:00Z".parse().expect("ts"),
        }
    }

    fn markets() -> MarketIndex {
        let rule = |ex: &str, m: &str, c: &[&str]| MarketRule {
            exchange_id: ex.into(),
            market_id: m.into(),
            clusters: c.iter().map(|s| (*s).to_owned()).collect(),
            verified: true,
            ..Default::default()
        };
        MarketIndex::new(&MarketMap {
            header: None,
            markets: vec![
                rule("1", "m1", &["national", "state:OH"]),
                rule("2", "m1", &["national", "state:OH"]),
                rule("3", "m3", &["national", "state:ME"]),
            ],
        })
    }

    fn spec(ex: &str, side: Side, action: Action, qty: i64, price: u32) -> OrderSpec {
        OrderSpec {
            intent_id: format!("t:{ex}"),
            strategy: "t".into(),
            exchange_id: ExchangeId::new(ex),
            market_id: None,
            side,
            action,
            quantity: Qty(qty),
            price: Pips::new(price).expect("valid"),
            ttl: Duration::from_secs(30),
            needle_derived: false,
            fair_yes: None,
        }
    }

    struct World {
        positions: HashMap<ExchangeId, i64>,
        marks: HashMap<ExchangeId, f64>,
        live: Vec<LiveOrder>,
        locked: HashSet<ExchangeId>,
        markets: MarketIndex,
        calendar: Calendar,
        tid: TournamentId,
    }

    impl World {
        fn new() -> Self {
            Self {
                positions: HashMap::new(),
                marks: HashMap::new(),
                live: vec![],
                locked: HashSet::new(),
                markets: markets(),
                calendar: calendar(),
                tid: TournamentId::new("t"),
            }
        }

        fn ctx(&self, equity: f64) -> RiskContext<'_> {
            RiskContext {
                now: "2026-10-10T12:00:00Z".parse().expect("ts"),
                equity,
                calendar: &self.calendar,
                tournament: &self.tid,
                positions: &self.positions,
                marks: &self.marks,
                live_orders: &self.live,
                markets: &self.markets,
                locked: &self.locked,
                glitch_used: 0.0,
                cash: None,
                bundle_cost: None,
            }
        }
    }

    fn check(w: &World, equity: f64, legs: Vec<OrderSpec>) -> Result<(), Rejection> {
        PreTrade { limits: limits() }.check_bundle(
            &w.ctx(equity),
            &OrderBundle { legs, atomic: false },
            &w.tid,
        )
    }

    #[test]
    fn per_order_and_per_market_caps() {
        let w = World::new();
        // 2 % of 10 000 = 200: 400 shares at 0.5 is too big for one order.
        assert!(matches!(
            check(&w, 10_000.0, vec![spec("1", Side::Yes, Action::Buy, 401, 5000)]),
            Err(Rejection::OrderNotional { .. })
        ));
        assert_eq!(
            check(&w, 10_000.0, vec![spec("1", Side::Yes, Action::Buy, 400, 5000)]),
            Ok(())
        );
        // Market m1 cap = 500: existing 800 YES at 0.5 on exchange 2 (same market) leaves 100.
        let mut w = World::new();
        w.positions.insert(ExchangeId::new("2"), 800);
        w.marks.insert(ExchangeId::new("2"), 0.5);
        assert_eq!(
            check(&w, 10_000.0, vec![spec("1", Side::Yes, Action::Buy, 200, 5000)]),
            Ok(())
        );
        assert!(matches!(
            check(&w, 10_000.0, vec![spec("1", Side::Yes, Action::Buy, 201, 5000)]),
            Err(Rejection::MarketCap { .. })
        ));
    }

    #[derive(Debug)]
    struct Flat(f64);

    impl BundleCost for Flat {
        fn cash_needed(&self, _: &HashMap<ExchangeId, i64>, _: &OrderBundle) -> f64 {
            self.0
        }
    }

    fn check_cash(
        w: &World,
        cash: f64,
        model: Option<&dyn BundleCost>,
        legs: Vec<OrderSpec>,
    ) -> Result<(), Rejection> {
        let mut ctx = w.ctx(100_000.0);
        ctx.cash = Some(cash);
        ctx.bundle_cost = model;
        PreTrade { limits: limits() }.check_bundle(&ctx, &OrderBundle { legs, atomic: true }, &w.tid)
    }

    #[test]
    fn buying_power_counts_full_opening_notional_by_default() {
        let w = World::new();
        let buy = || vec![spec("1", Side::Yes, Action::Buy, 20, 5000)];
        assert_eq!(check_cash(&w, 10.0, None, buy()), Ok(()));
        assert!(matches!(
            check_cash(&w, 9.99, None, buy()),
            Err(Rejection::BuyingPower { .. })
        ));
        // Unknown cash: left to the platform.
        assert_eq!(check(&w, 100_000.0, buy()), Ok(()));
    }

    #[test]
    fn buying_power_reserves_live_orders_and_frees_closing_shares() {
        let mut w = World::new();
        w.live.push(LiveOrder {
            exchange_id: ExchangeId::new("3"),
            book_side: BookSide::Ask,
            yes_price: Pips::new(6000).expect("valid"),
            remaining: 10, // a resting NO buy at 0.40 reserves 4.0
        });
        let buy = || vec![spec("1", Side::Yes, Action::Buy, 20, 5000)];
        assert!(matches!(
            check_cash(&w, 13.0, None, buy()),
            Err(Rejection::BuyingPower { .. })
        ));
        assert_eq!(check_cash(&w, 14.0, None, buy()), Ok(()));
        // Buying NO against a YES position closes it: it needs no cash even when over-committed.
        w.positions.insert(ExchangeId::new("2"), 30);
        assert_eq!(
            check_cash(&w, 0.0, None, vec![spec("2", Side::No, Action::Buy, 30, 5000)]),
            Ok(())
        );
    }

    #[test]
    fn a_collateral_cost_model_replaces_full_notional() {
        let w = World::new();
        let legs = || {
            vec![
                spec("1", Side::No, Action::Buy, 10, 6000),
                spec("2", Side::No, Action::Buy, 10, 4500),
            ]
        };
        assert!(matches!(
            check_cash(&w, 7.0, None, legs()),
            Err(Rejection::BuyingPower { .. })
        ));
        assert_eq!(check_cash(&w, 7.0, Some(&Flat(6.0)), legs()), Ok(()));
        assert_eq!(opening_notional(&HashMap::new(), &legs()), 10.5);
    }

    #[test]
    fn reducing_exposure_is_always_allowed() {
        let mut w = World::new();
        w.positions.insert(ExchangeId::new("1"), 5_000);
        w.marks.insert(ExchangeId::new("1"), 0.5);
        // Way over every cap already; buying NO nets against the YES and must pass.
        assert_eq!(
            check(&w, 10_000.0, vec![spec("1", Side::No, Action::Buy, 100, 5000)]),
            Ok(())
        );
        assert!(check(&w, 10_000.0, vec![spec("1", Side::Yes, Action::Buy, 1, 5000)]).is_err());
    }

    #[test]
    fn locked_legs_cannot_be_reduced() {
        let mut w = World::new();
        w.positions.insert(ExchangeId::new("3"), 100);
        w.locked.insert(ExchangeId::new("3"));
        assert_eq!(
            check(&w, 100_000.0, vec![spec("3", Side::Yes, Action::Sell, 10, 5000)]),
            Err(Rejection::LockedLeg(ExchangeId::new("3")))
        );
        assert_eq!(
            check(&w, 100_000.0, vec![spec("3", Side::Yes, Action::Buy, 10, 5000)]),
            Ok(())
        );
    }

    #[test]
    fn cluster_cap_uses_the_tightest_limit() {
        let mut w = World::new();
        w.positions.insert(ExchangeId::new("3"), 1_900);
        w.marks.insert(ExchangeId::new("3"), 0.5);
        let mut pt = PreTrade { limits: limits() };
        pt.limits.per_market_gross_frac = 1.0;
        let ctx = w.ctx(10_000.0);
        let b = |q| OrderBundle {
            legs: vec![spec("3", Side::Yes, Action::Buy, q, 5000)],
            atomic: false,
        };
        // state:ME kind cap = 1 000: 950 held + 25 more fits; an override of 9 % (900) does not.
        assert_eq!(pt.check_bundle(&ctx, &b(50), &w.tid), Ok(()));
        pt.limits.cluster_override_frac.insert("state:ME".into(), 0.09);
        assert!(matches!(
            pt.check_bundle(&ctx, &b(50), &w.tid),
            Err(Rejection::ClusterCap { .. })
        ));
    }

    #[test]
    fn gates_and_self_cross() {
        let mut w = World::new();
        let late = PreTrade { limits: limits() };
        let mut ctx = w.ctx(10_000.0);
        ctx.now = "2026-11-04T16:55:00Z".parse().expect("ts");
        let b = OrderBundle::single(spec("1", Side::Yes, Action::Buy, 1, 5000));
        assert_eq!(late.check_bundle(&ctx, &b, &w.tid), Err(Rejection::TimeGate));
        assert!(matches!(
            late.check_bundle(&w.ctx(10_000.0), &b, &TournamentId::new("other")),
            Err(Rejection::WrongTournament { .. })
        ));
        w.live.push(LiveOrder {
            exchange_id: ExchangeId::new("1"),
            book_side: BookSide::Ask,
            yes_price: Pips::new(5000).expect("valid"),
            remaining: 3,
        });
        // Our resting YES ask at 0.50; a YES bid at 0.50 would cross it.
        assert_eq!(
            check(&w, 10_000.0, vec![spec("1", Side::Yes, Action::Buy, 1, 5000)]),
            Err(Rejection::SelfCross(ExchangeId::new("1")))
        );
        assert_eq!(
            check(&w, 10_000.0, vec![spec("1", Side::Yes, Action::Buy, 1, 4950)]),
            Ok(())
        );
    }

    #[test]
    fn glitch_trades_are_capped() {
        let w = World::new();
        let mut leg = spec("1", Side::Yes, Action::Buy, 300, 3000);
        leg.fair_yes = Some(0.50); // 20 pts through fair
        let pt = PreTrade { limits: limits() };
        assert!(pt.is_glitch(&leg));
        let mut ctx = w.ctx(10_000.0);
        ctx.glitch_used = 150.0;
        // 90 more on top of 150 used > 200 cap.
        assert!(matches!(
            pt.check_bundle(&ctx, &OrderBundle::single(leg), &w.tid),
            Err(Rejection::GlitchCap { .. })
        ));
    }
}
