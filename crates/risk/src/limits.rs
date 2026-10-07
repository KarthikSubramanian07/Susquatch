//! Pre-trade checks: caps, glitch cap, locked legs, time gate, tournament, self-cross.
//!
//! Exposure is measured as money committed, the way the bridge measures it: a position is worth
//! `|net shares| × price of the held side` (YES at the YES mark, NO at `1 − mark`); every live
//! order counts the full notional of its unfilled remainder at its limit, as if it fills; a new
//! order counts only the part that opens exposure, at its limit (shares that close a position pay
//! out and add nothing). Valuing resting orders at their limit rather than at the mark is what
//! lets a far quote (a NO bid at 0.01 on a market marked 0.02) cost 0.01 a share, not 0.98. An
//! order that opens nothing is never blocked by a cap (reducing risk is always allowed, except on
//! locked legs).
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
use serde::Serialize;
use susq_core::{
    Action, BookSide, Calendar, ExchangeId, MarketIndex, OrderBundle, OrderSpec, Pips, TournamentId,
    config::RiskCfg, market_map::cluster_kind,
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
    #[error("election-night budget: {after:.2} > {cap:.2}")]
    NightBudget { after: f64, cap: f64 },
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
    /// Glitch threshold for needle-derived legs (`[strategies.en] en_glitch_pts`), whose
    /// `fair_yes` is the needle posterior: a post-batch edge is a 15+ point buy by design, so
    /// the general threshold would spend the whole glitch cap on the first good trades.
    pub needle_glitch_through_fair: f64,
    /// Most equity needle-derived buys may have deployed at once (`[strategies.en]
    /// night_budget_frac`), counted by [`RiskContext::needle_used`]. `None`: no night budget.
    pub needle_budget_frac: Option<f64>,
}

impl RiskLimits {
    /// From `[risk]`. Needle-derived legs use the general glitch threshold until
    /// [`RiskLimits::with_needle_glitch_pts`] sets their own.
    pub fn from_cfg(cfg: &RiskCfg) -> Self {
        Self {
            per_market_gross_frac: cfg.per_market_gross_frac,
            total_gross_frac: cfg.total_gross_frac,
            cluster_kind_frac: cfg.clusters.clone(),
            cluster_override_frac: BTreeMap::new(),
            max_order_notional_frac: cfg.max_order_notional_frac,
            glitch_through_fair: cfg.glitch_through_fair_pts / 100.0,
            glitch_max_frac: cfg.glitch_max_frac,
            needle_glitch_through_fair: cfg.glitch_through_fair_pts / 100.0,
            needle_budget_frac: None,
        }
    }

    /// Glitch threshold (points) for needle-derived legs (`[strategies.en] en_glitch_pts`).
    pub fn with_needle_glitch_pts(mut self, pts: f64) -> Self {
        self.needle_glitch_through_fair = pts / 100.0;
        self
    }

    /// Night budget for needle-derived buys (`[strategies.en] night_budget_frac`).
    pub fn with_needle_budget_frac(mut self, frac: f64) -> Self {
        self.needle_budget_frac = Some(frac);
        self
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
    /// SUSQies needle-derived orders have deployed: bought at cost plus resting buys at their
    /// limit, less what rolls sold back (the live loop computes it from the OMS).
    pub needle_used: f64,
    /// Tournament cash, when known (`None`: buying power is left to the platform).
    pub cash: Option<f64>,
    /// Collateral-aware bundle cost; `None` = full opening notional (the default, conservative).
    pub bundle_cost: Option<&'a dyn BundleCost>,
}

/// What the caps saw when they judged a bundle (SUSQies), for the journal and the log: the same
/// fields the bridge's `Decision.detail` carries, so one reader explains both traders' refusals.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RiskDetail {
    pub equity: f64,
    /// Gross exposure before the bundle: positions at mark plus every live order at its limit.
    pub total: f64,
    /// The part of `total` already on the markets the bundle touches.
    pub market: f64,
    /// Exchanges with a non-zero position.
    pub positions: usize,
    /// Live orders counted against the caps.
    pub resting: usize,
}

/// Signed position after every live order fills, per exchange.
fn projected_positions(ctx: &RiskContext<'_>) -> HashMap<ExchangeId, i64> {
    let mut out = ctx.positions.clone();
    for o in ctx.live_orders {
        *out.entry(o.exchange_id.clone()).or_default() += o.book_side.yes_delta_sign() * o.remaining;
    }
    out
}

/// Full notional of the shares a bundle opens, per exchange (closing shares, which net against a
/// position and pay out, are free), legs applied in order.
fn opening_by_exchange(positions: &HashMap<ExchangeId, i64>, legs: &[OrderSpec]) -> HashMap<ExchangeId, f64> {
    let mut held = positions.clone();
    let mut out: HashMap<ExchangeId, f64> = HashMap::new();
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
        *out.entry(leg.exchange_id.clone()).or_default() += (delta.abs() - closing) as f64 * open_px;
        *held.entry(leg.exchange_id.clone()).or_default() += delta;
    }
    out
}

/// Full notional of the shares a bundle opens (closing shares, which net against a position and
/// pay out, are free), legs applied in order.
pub fn opening_notional(positions: &HashMap<ExchangeId, i64>, legs: &[OrderSpec]) -> f64 {
    opening_by_exchange(positions, legs).values().sum()
}

/// Money at risk of a signed position at a YES mark.
fn exposure(net: i64, yes_mark: f64) -> f64 {
    let px = if net >= 0 { yes_mark } else { 1.0 - yes_mark };
    net.unsigned_abs() as f64 * px.clamp(0.0, 1.0)
}

/// Cash a live order consumes if it fills: the unfilled remainder at the limit, on the side it buys.
fn order_notional(o: &LiveOrder) -> f64 {
    let px = match o.book_side {
        BookSide::Bid => o.yes_price.prob(),
        BookSide::Ask => 1.0 - o.yes_price.prob(),
    };
    o.remaining.max(0) as f64 * px
}

/// Money committed per exchange before a bundle: positions at mark plus live orders at their limit.
fn committed(ctx: &RiskContext<'_>, mark: &dyn Fn(&ExchangeId) -> f64) -> HashMap<ExchangeId, f64> {
    let mut out: HashMap<ExchangeId, f64> = HashMap::new();
    for (ex, net) in ctx.positions {
        if *net != 0 {
            out.insert(ex.clone(), exposure(*net, mark(ex)));
        }
    }
    for o in ctx.live_orders {
        *out.entry(o.exchange_id.clone()).or_default() += order_notional(o);
    }
    out
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
        let opening = opening_by_exchange(ctx.positions, &bundle.legs);
        let increases: Vec<&ExchangeId> = touched
            .iter()
            .copied()
            .filter(|ex| opening.get(*ex).is_some_and(|add| *add > 1e-9))
            .collect();
        self.check_buying_power(ctx, bundle)?;
        if increases.is_empty() {
            return Ok(());
        }
        self.check_glitch(ctx, &bundle.legs)?;
        self.check_night_budget(ctx, &bundle.legs)?;
        let mut exposure_after = committed(ctx, &mark);
        for (ex, add) in opening {
            *exposure_after.entry(ex).or_default() += add;
        }
        self.check_caps(ctx, &exposure_after, &increases)
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
        let reserved: f64 = ctx.live_orders.iter().map(order_notional).sum();
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

    /// Needle-derived buys may not take the night's deployment past `needle_budget_frac`.
    /// Sells (rolls) only ever free budget, so they are never refused here.
    fn check_night_budget(&self, ctx: &RiskContext<'_>, legs: &[OrderSpec]) -> Result<(), Rejection> {
        let Some(frac) = self.limits.needle_budget_frac else {
            return Ok(());
        };
        let add: f64 = legs
            .iter()
            .filter(|l| l.needle_derived && l.action == Action::Buy)
            .map(OrderSpec::max_notional)
            .sum();
        if add <= 0.0 {
            return Ok(());
        }
        let cap = frac * ctx.equity;
        let after = ctx.needle_used.max(0.0) + add;
        if after > cap + 1e-9 {
            return Err(Rejection::NightBudget { after, cap });
        }
        Ok(())
    }

    /// An order priced `glitch_through_fair` or more through fair in our favour
    /// (`needle_glitch_through_fair` for a needle-derived leg, whose fair is the posterior).
    pub fn is_glitch(&self, leg: &OrderSpec) -> bool {
        let Some(fair_yes) = leg.fair_yes else {
            return false;
        };
        let (side, yes_price) = leg.yes_space();
        let through = match side {
            BookSide::Bid => fair_yes - yes_price.prob(),
            BookSide::Ask => yes_price.prob() - fair_yes,
        };
        let threshold = if leg.needle_derived {
            self.limits.needle_glitch_through_fair
        } else {
            self.limits.glitch_through_fair
        };
        through >= threshold - 1e-9
    }

    /// `after` is the money committed per exchange once the bundle fills.
    fn check_caps(
        &self,
        ctx: &RiskContext<'_>,
        after: &HashMap<ExchangeId, f64>,
        increases: &[&ExchangeId],
    ) -> Result<(), Rejection> {
        let eq = ctx.equity;
        let exp = |ex: &ExchangeId| after.get(ex).copied().unwrap_or(0.0);

        let gross: f64 = after.values().sum();
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

    /// The exposure picture a refusal of `bundle` should be read against. Marks fall back to the
    /// bundle's own prices and then 0.5, exactly as [`PreTrade::check_bundle`] values them.
    pub fn detail(&self, ctx: &RiskContext<'_>, bundle: &OrderBundle) -> RiskDetail {
        let mut order_marks: HashMap<&ExchangeId, f64> = HashMap::new();
        for leg in &bundle.legs {
            order_marks
                .entry(&leg.exchange_id)
                .or_insert(leg.yes_space().1.prob());
        }
        let mark = |ex: &ExchangeId| {
            ctx.marks
                .get(ex)
                .copied()
                .or_else(|| order_marks.get(ex).copied())
                .unwrap_or(0.5)
        };
        let market_key = |ex: &ExchangeId| {
            ctx.markets
                .market_of(ex)
                .map_or_else(|| format!("exchange:{ex}"), |m| format!("market:{m}"))
        };
        let touched: HashSet<String> = bundle.legs.iter().map(|l| market_key(&l.exchange_id)).collect();
        let (mut total, mut market) = (0.0, 0.0);
        for (ex, e) in committed(ctx, &mark) {
            total += e;
            if touched.contains(&market_key(&ex)) {
                market += e;
            }
        }
        let cents = |x: f64| (x * 100.0).round() / 100.0;
        RiskDetail {
            equity: cents(ctx.equity),
            total: cents(total),
            market: cents(market),
            positions: ctx.positions.values().filter(|q| **q != 0).count(),
            resting: ctx.live_orders.len(),
        }
    }

    /// Current cluster utilisation (exposure / cap) for every cluster in use, for MM skew and status.
    pub fn cluster_utilization(&self, ctx: &RiskContext<'_>) -> HashMap<String, f64> {
        let mark = |ex: &ExchangeId| ctx.marks.get(ex).copied().unwrap_or(0.5);
        let mut totals: HashMap<String, f64> = HashMap::new();
        for (ex, e) in committed(ctx, &mark) {
            for c in ctx.markets.clusters(&ex) {
                *totals.entry(c.clone()).or_default() += e;
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
            needle_glitch_through_fair: 0.40,
            needle_budget_frac: None,
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
                needle_used: 0.0,
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
    fn detail_reports_what_the_caps_saw() {
        let mut w = World::new();
        // 800 YES at 0.5 on exchange 2 (market m1) and 100 NO at a 0.3 mark on exchange 3 (m3).
        w.positions.insert(ExchangeId::new("2"), 800);
        w.marks.insert(ExchangeId::new("2"), 0.5);
        w.positions.insert(ExchangeId::new("3"), -100);
        w.marks.insert(ExchangeId::new("3"), 0.3);
        // A resting YES bid of 10 at 0.40 on exchange 1 (m1) counts as if it fills.
        w.live.push(LiveOrder {
            exchange_id: ExchangeId::new("1"),
            book_side: BookSide::Bid,
            yes_price: Pips::new(4000).expect("valid"),
            remaining: 10,
        });
        let pt = PreTrade { limits: limits() };
        let bundle = OrderBundle::single(spec("1", Side::Yes, Action::Buy, 201, 5000));
        let d = pt.detail(&w.ctx(10_000.0), &bundle);
        assert_eq!(d.equity, 10_000.0);
        // 400 (ex 2) + 70 (ex 3 at 1 - 0.3) + 4 (ex 1: the resting bid at its 0.40 limit).
        assert_eq!(d.total, 474.0);
        assert_eq!(d.market, 404.0, "m1 only: exchanges 1 and 2");
        assert_eq!((d.positions, d.resting), (2, 1));
        assert!(
            matches!(
                check(&w, 10_000.0, bundle.legs.clone()),
                Err(Rejection::MarketCap { .. })
            ),
            "the detail explains a market-cap refusal"
        );
        let json = serde_json::to_value(&d).expect("json");
        assert_eq!(json["market"], 404.0);
        assert_eq!(json["resting"], 1);
    }

    /// A far quote is money committed at its limit, not the share it might win marked at 0.98.
    #[test]
    fn resting_and_new_orders_count_at_their_limit_not_at_the_mark() {
        let mut w = World::new();
        w.marks.insert(ExchangeId::new("3"), 0.02);
        // 2 % of 10 000 = 200 per order: 20 000 NO at 0.01. At the mark that "position" would be
        // 19 600 of exposure; committed it is 200, inside the 500 market cap.
        let rung = |q| vec![spec("3", Side::No, Action::Buy, q, 100)];
        assert_eq!(check(&w, 10_000.0, rung(20_000)), Ok(()));
        // Two rungs already resting (200 + 180) leave 120: a third of 200 is over, one of 100 fits.
        for (yes_price, remaining) in [(9900, 20_000), (9700, 6_000)] {
            w.live.push(LiveOrder {
                exchange_id: ExchangeId::new("3"),
                book_side: BookSide::Ask,
                yes_price: Pips::new(yes_price).expect("valid"),
                remaining,
            });
        }
        assert!(matches!(
            check(&w, 10_000.0, rung(20_000)),
            Err(Rejection::MarketCap { after, cap, .. }) if (after - 580.0).abs() < 1e-9 && cap == 500.0
        ));
        assert_eq!(check(&w, 10_000.0, rung(10_000)), Ok(()));
        let util = PreTrade { limits: limits() }.cluster_utilization(&w.ctx(10_000.0));
        assert!((util["state:ME"] - 0.38).abs() < 1e-9, "{util:?}");
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

    /// Gap 9 of the readiness audit: a needle-derived buy 20 points through its posterior is the
    /// trade the election-night executor exists for, not a glitch; its own threshold applies.
    #[test]
    fn needle_derived_legs_have_their_own_glitch_threshold() {
        let w = World::new();
        let pt = PreTrade { limits: limits() };
        let mut ctx = w.ctx(10_000.0);
        ctx.glitch_used = 150.0;
        let mut leg = spec("1", Side::Yes, Action::Buy, 300, 3000);
        leg.fair_yes = Some(0.50); // 20 pts through the posterior
        leg.needle_derived = true;
        assert!(!pt.is_glitch(&leg), "20 pts is under the 40 pt needle threshold");
        assert_eq!(
            pt.check_bundle(&ctx, &OrderBundle::single(leg.clone()), &w.tid),
            Ok(()),
            "the general glitch cap no longer bites"
        );
        // The same edge on the NO side: a NO buy at 0.30 (YES ask 0.70) against a posterior of 0.50.
        let mut no = spec("1", Side::No, Action::Buy, 300, 3000);
        no.fair_yes = Some(0.50);
        no.needle_derived = true;
        assert!(!pt.is_glitch(&no));
        // 45 pts through the posterior is a glitch even for the needle (inverted map, broken model).
        leg.fair_yes = Some(0.75);
        assert!(pt.is_glitch(&leg));
        assert!(matches!(
            pt.check_bundle(&ctx, &OrderBundle::single(leg.clone()), &w.tid),
            Err(Rejection::GlitchCap { .. })
        ));
        // Exactly at the threshold counts (same rule as the general cap).
        leg.fair_yes = Some(0.70);
        assert!(pt.is_glitch(&leg));
        // Without the builder, needle legs fall under the general threshold.
        let plain = PreTrade {
            limits: RiskLimits::from_cfg(&RiskCfg {
                per_market_gross_frac: 0.05,
                total_gross_frac: 0.6,
                glitch_through_fair_pts: 15.0,
                glitch_max_frac: 0.02,
                loss_per_hour_frac: 0.03,
                reject_breaker_per_min: 10,
                max_order_notional_frac: 0.02,
                clusters: BTreeMap::new(),
                bus_error_breaker_per_min: 20,
                use_collateral_buying_power: Default::default(),
            }),
        };
        leg.fair_yes = Some(0.50);
        assert!(plain.is_glitch(&leg));
        assert_eq!(plain.limits.needle_glitch_through_fair, 0.15);
        let built = PreTrade {
            limits: plain.limits.clone().with_needle_glitch_pts(40.0),
        };
        assert!(!built.is_glitch(&leg));
    }

    /// Gap 10 of the readiness audit: the election-night executor deploys at most
    /// `night_budget_frac` of equity; rolls (sells) always pass, other strategies are untouched.
    #[test]
    fn night_budget_caps_needle_buys_only() {
        let w = World::new();
        let pt = PreTrade {
            limits: limits()
                .with_needle_glitch_pts(40.0)
                .with_needle_budget_frac(0.25),
        };
        let mut ctx = w.ctx(10_000.0);
        let mut buy = spec("1", Side::Yes, Action::Buy, 300, 3000); // 90 SUSQies
        buy.needle_derived = true;
        buy.fair_yes = Some(0.45);
        ctx.needle_used = 2_400.0;
        assert_eq!(
            pt.check_bundle(&ctx, &OrderBundle::single(buy.clone()), &w.tid),
            Ok(())
        );
        ctx.needle_used = 2_450.0; // 2,450 + 90 > 2,500
        assert!(matches!(
            pt.check_bundle(&ctx, &OrderBundle::single(buy.clone()), &w.tid),
            Err(Rejection::NightBudget { .. })
        ));
        // The same buy from a strategy that is not the needle's is not the night budget's business.
        let mut sniper = buy.clone();
        sniper.needle_derived = false;
        sniper.fair_yes = None;
        assert_eq!(
            pt.check_bundle(&ctx, &OrderBundle::single(sniper), &w.tid),
            Ok(())
        );
        // No night budget configured: needle buys pass on the other caps alone.
        let open = PreTrade {
            limits: limits().with_needle_glitch_pts(40.0),
        };
        assert_eq!(open.check_bundle(&ctx, &OrderBundle::single(buy), &w.tid), Ok(()));
    }
}
