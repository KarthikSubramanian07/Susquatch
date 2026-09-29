//! [`MockPlatform`]: an in-memory [`PlatformAdapter`] backed by a small but real matching engine.
//!
//! Every rust-live integration test (gateway retries, OMS no-double-send, paper mode) runs against
//! it, so it models the semantics in `docs/API.md` instead of returning canned data:
//!
//! * One YES-space book per exchange, price-time priority, trades at the resting order's price.
//! * Sell canonicalization (`sell yes q@p` == `buy no q@(1-p)` unless fully backed by the held
//!   position), self-trade prevention (the crossing remainder is cancelled with
//!   `SelfTradePrevented`), netting of opposite buys (each YES/NO pair pays 1) and lazy expiry.
//! * Idempotency on every write: the same key with the same payload replays the stored response
//!   without executing again; a different payload is `409 CONFLICT`. Only successful executions
//!   are stored.
//! * One-shot fault injection per [`Endpoint`] and per-endpoint call counters.
//! * ALL collateral (with `MockConfig::collateral`, the default), after
//!   [`MockPlatform::set_relationships`]: fills on linked exchanges get the advance the shared model
//!   ([`crate::collateral`], a port of the Python mock's) grants, `all` is present exactly for linked
//!   exchanges, and `GET /portfolio/collateral` serves the modeled components. Kept minimal on
//!   purpose: the balance check still reserves full notional (no credit for a prospective advance)
//!   and a repayment may overdraw cash; the Python mock (`py/susq/mock`) is the reference model.
//!
//! Accounting choices:
//! * Positions use average cost (the platform's FIFO lots are not modeled; `lots` is empty).
//! * Marks: the exchange's last trade price, else the book mid, else the position's average cost.
//!   A NO position is marked at `1 - YES mark`. `totalAccountValue = cash + Σ |qty| × mark`.
//! * Resting buys reserve `remaining × price` of cash for the balance check.
//! * Fills are outcome-signed from the owner's side: `+q`/YES for a bid fill, `-q`/NO for an ask
//!   fill, at the YES price.
//!
//! The API caller is the owner [`CALLER`]; liquidity seeded for other owners has unlimited balance.

mod api;
mod engine;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use susq_core::{Action, Clock, ExchangeId, MarketId, OrderId, Pips, Side, TournamentId};

use crate::{
    adapter::PlatformAdapter,
    error::{ApiError, PlatformError, PlatformResult},
    models::*,
};
use engine::{State, Submission};

/// Owner name of the API caller in the mock's engine.
pub const CALLER: &str = "me";

/// Static configuration of a [`MockPlatform`].
#[derive(Debug, Clone, PartialEq)]
pub struct MockConfig {
    /// Tournament UUID every tournament-scoped call must pass.
    pub tournament_id: String,
    /// Slug the `/tournaments/{slug}/…` calls must use.
    pub slug: String,
    /// The caller's starting cash.
    pub starting_balance: f64,
    /// `memberCount` reported by `GET /tournaments/{slug}`.
    pub member_count: u32,
    /// Rows per page of every paginated listing (small values exercise pagination).
    pub page_size: usize,
    /// Model ALL collateral advances (false: plain cash accounting and `all: null`).
    pub collateral: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            tournament_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            slug: "midterm-elections".to_owned(),
            starting_balance: 1_000.0,
            member_count: 100,
            page_size: 200,
            collateral: true,
        }
    }
}

/// A [`PlatformAdapter`] method, for fault injection and call counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Tournament,
    Markets,
    Prices,
    Orderbook,
    PlaceOrder,
    Batch,
    MultiLeg,
    Cancel,
    CancelAll,
    Orders,
    Positions,
    Pnl,
    Fills,
    Leaderboard,
    Relationships,
    Constraints,
    RealtimeToken,
    Collateral,
}

/// A one-shot failure for the next call of an [`Endpoint`].
#[derive(Debug, Clone, PartialEq)]
pub enum Fault {
    /// `429 RATE_LIMITED`; nothing executes.
    RateLimited { retry_after: Option<Duration> },
    /// The call executes for real (and its idempotent response is stored), then answers
    /// `502 ORDER_STATUS_UNKNOWN`.
    UnknownButPlaced,
    /// `503 SERVICE_UNAVAILABLE`; nothing executes.
    Unavailable,
    /// `409 REQUEST_IN_FLIGHT`; nothing executes.
    InFlight,
    /// Transport failure, optionally after the call executed.
    Network { executed: bool },
    /// `500 INTERNAL_ERROR`; nothing executes.
    Internal,
}

impl Fault {
    /// The error returned instead of executing, if this fault prevents execution.
    fn before(&self) -> Option<PlatformError> {
        let err = match self {
            Fault::RateLimited { retry_after } => {
                let e = ApiError::new(429, "RATE_LIMITED", "rate limit exceeded (injected)");
                match retry_after {
                    Some(d) => e.with_retry_after(*d),
                    None => e,
                }
                .into()
            }
            Fault::Unavailable => {
                ApiError::new(503, "SERVICE_UNAVAILABLE", "service unavailable (injected)").into()
            }
            Fault::InFlight => {
                ApiError::new(409, "REQUEST_IN_FLIGHT", "identical request in flight (injected)").into()
            }
            Fault::Internal => ApiError::new(500, "INTERNAL_ERROR", "internal error (injected)").into(),
            Fault::Network { executed: false } => network_error(),
            Fault::UnknownButPlaced | Fault::Network { executed: true } => return None,
        };
        Some(err)
    }

    /// The error returned after executing, if this fault hides the outcome.
    fn after(&self) -> Option<PlatformError> {
        match self {
            Fault::UnknownButPlaced => {
                Some(ApiError::new(502, "ORDER_STATUS_UNKNOWN", "engine did not confirm (injected)").into())
            }
            Fault::Network { executed: true } => Some(network_error()),
            _ => None,
        }
    }
}

fn network_error() -> PlatformError {
    PlatformError::Network {
        message: "connection reset (injected)".to_owned(),
        timeout: false,
    }
}

/// Engine state plus the test instrumentation around it.
#[derive(Debug)]
struct Inner {
    state: State,
    faults: HashMap<Endpoint, VecDeque<Fault>>,
    calls: HashMap<Endpoint, usize>,
}

/// In-memory platform with a real matching engine. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct MockPlatform {
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

impl MockPlatform {
    /// An empty platform: no markets, the caller holding `cfg.starting_balance` in cash.
    pub fn new(clock: Arc<dyn Clock>, cfg: MockConfig) -> Self {
        Self {
            clock,
            inner: Mutex::new(Inner {
                state: State::new(cfg),
                faults: HashMap::new(),
                calls: HashMap::new(),
            }),
        }
    }

    // ───────────────────────────── seeding ─────────────────────────────

    /// Register an open market with its `(exchange id, option label)` pairs.
    pub fn add_market(&self, market_id: impl Into<MarketId>, title: &str, exchanges: &[(&str, &str)]) {
        let exchanges: Vec<(ExchangeId, String)> = exchanges
            .iter()
            .map(|(id, option)| (ExchangeId::new(*id), (*option).to_owned()))
            .collect();
        self.inner
            .lock()
            .state
            .add_market(market_id.into(), title, &exchanges);
    }

    /// Place an order for `owner` straight into the engine (no balance check, no expiry), e.g.
    /// other participants' liquidity. It matches, canonicalizes and self-trade-prevents exactly
    /// like an API order.
    ///
    /// # Panics
    /// If `qty` is not positive (a test-setup bug).
    pub fn seed_order(
        &self,
        owner: &str,
        exchange: impl Into<ExchangeId>,
        side: Side,
        action: Action,
        qty: i64,
        price: Pips,
    ) -> OrderId {
        assert!(qty > 0, "seeded order quantity must be positive, got {qty}");
        let now = self.clock.now();
        let sub = Submission {
            owner: owner.to_owned(),
            exchange: exchange.into(),
            side,
            action,
            quantity: qty,
            price,
            expiration: None,
        };
        let mut inner = self.inner.lock();
        inner.state.expire(now);
        inner.state.submit(sub, now).0
    }

    /// Replace `owner`'s resting liquidity on `exchange` with the given YES-space levels
    /// (`(price, qty)`; non-positive sizes are skipped). New levels match against everyone else's
    /// resting orders exactly like incoming orders, which is how the paper broker fills our resting
    /// paper orders when the live book moves through them. Unknown exchanges are registered under
    /// a placeholder market. Returns the number of levels placed.
    pub fn replace_liquidity(
        &self,
        owner: &str,
        exchange: &ExchangeId,
        bids: &[(Pips, i64)],
        asks: &[(Pips, i64)],
    ) -> usize {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let state = &mut inner.state;
        state.expire(now);
        if !state.exchanges.contains_key(exchange) {
            let market = MarketId::new(format!("unmapped-{exchange}"));
            state.add_market(market, "", &[(exchange.clone(), String::new())]);
        }
        state.cancel_open(owner, |o| &o.exchange == exchange);
        let levels = bids
            .iter()
            .map(|l| (Action::Buy, l))
            .chain(asks.iter().map(|l| (Action::Sell, l)))
            .filter(|(_, (_, qty))| *qty > 0);
        let mut placed = 0;
        for (action, &(price, quantity)) in levels {
            let sub = Submission {
                owner: owner.to_owned(),
                exchange: exchange.clone(),
                side: Side::Yes,
                action,
                quantity,
                price,
                expiration: None,
            };
            state.submit(sub, now);
            placed += 1;
        }
        placed
    }

    /// Replace the canonical relationships served by `GET /relationships` (and the ALL collateral
    /// components built from them).
    pub fn set_relationships(&self, relationships: Vec<Relationship>) {
        self.inner.lock().state.set_relationships(relationships);
    }

    /// Replace the evaluations served by `GET /relationships/constraints`.
    pub fn set_constraints(&self, constraints: Vec<ConstraintEval>) {
        self.inner.lock().state.constraints = constraints;
    }

    /// Replace the leaderboard.
    pub fn set_leaderboard(&self, leaderboard: Leaderboard) {
        self.inner.lock().state.leaderboard = leaderboard;
    }

    /// Replace the token returned by `POST /realtime/token`.
    pub fn set_realtime_token(&self, token: RealtimeToken) {
        self.inner.lock().state.realtime_token = token;
    }

    /// Serve canned collateral components instead of the modeled ones.
    pub fn set_collateral(&self, collateral: Collateral) {
        self.inner.lock().state.collateral_override = Some(collateral);
    }

    // ───────────────────────────── faults & inspection ─────────────────────────────

    /// Queue a one-shot `fault` for the next call of `endpoint` (FIFO per endpoint).
    pub fn inject(&self, endpoint: Endpoint, fault: Fault) {
        self.inner
            .lock()
            .faults
            .entry(endpoint)
            .or_default()
            .push_back(fault);
    }

    /// How many times `endpoint` was called, faulted calls included.
    pub fn calls(&self, endpoint: Endpoint) -> usize {
        self.inner.lock().calls.get(&endpoint).copied().unwrap_or(0)
    }

    /// Distinct orders the engine actually created for [`CALLER`] (replays create none).
    pub fn engine_orders_created(&self) -> usize {
        self.inner.lock().state.caller_orders_created
    }

    /// Resting (unexpired) orders of `owner`, oldest first.
    pub fn open_orders_of(&self, owner: &str) -> Vec<Order> {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        inner.state.expire(now);
        inner
            .state
            .orders_of(owner)
            .filter(|o| o.is_open())
            .map(|o| o.to_wire())
            .collect()
    }

    /// Signed YES position (+YES / -NO) of `owner` on `exchange`.
    pub fn position_of(&self, owner: &str, exchange: impl Into<ExchangeId>) -> i64 {
        self.inner.lock().state.position(owner, &exchange.into())
    }

    /// Cash of `owner` (the caller starts at the configured balance, everyone else at 0).
    pub fn cash_of(&self, owner: &str) -> f64 {
        self.inner.lock().state.cash_of(owner)
    }

    /// Count the call, expire orders, apply any queued fault around `op`.
    fn run<T>(
        &self,
        endpoint: Endpoint,
        op: impl FnOnce(&mut State, DateTime<Utc>) -> PlatformResult<T>,
    ) -> PlatformResult<T> {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        *inner.calls.entry(endpoint).or_default() += 1;
        inner.state.expire(now);
        let fault = inner.faults.get_mut(&endpoint).and_then(VecDeque::pop_front);
        if let Some(err) = fault.as_ref().and_then(Fault::before) {
            return Err(err);
        }
        let result = op(&mut inner.state, now);
        match fault.as_ref().and_then(Fault::after) {
            Some(err) => Err(err),
            None => result,
        }
    }
}

#[async_trait]
impl PlatformAdapter for MockPlatform {
    async fn tournament(&self, slug: &str) -> PlatformResult<Tournament> {
        self.run(Endpoint::Tournament, |s, _| s.tournament(slug))
    }

    async fn markets_page(
        &self,
        slug: &str,
        status: MarketStatusFilter,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Market>> {
        self.run(Endpoint::Markets, |s, _| s.markets_page(slug, status, cursor))
    }

    async fn prices(&self, tid: &TournamentId, ids: &[ExchangeId]) -> PlatformResult<BulkPrices> {
        self.run(Endpoint::Prices, |s, _| s.prices(tid, ids))
    }

    async fn orderbook(
        &self,
        tid: &TournamentId,
        exchange: &ExchangeId,
        depth: u32,
    ) -> PlatformResult<OrderBook> {
        self.run(Endpoint::Orderbook, |s, _| s.orderbook(tid, exchange, depth))
    }

    async fn place_order(&self, req: &PlaceOrderRequest) -> PlatformResult<OrderAck> {
        self.run(Endpoint::PlaceOrder, |s, now| s.place(req, now))
    }

    async fn place_batch(&self, req: &BatchRequest) -> PlatformResult<BatchResponse> {
        self.run(Endpoint::Batch, |s, now| s.batch(req, now))
    }

    async fn place_multi_leg(&self, req: &MultiLegRequest) -> PlatformResult<MultiLegResponse> {
        self.run(Endpoint::MultiLeg, |s, now| s.multi_leg(req, now))
    }

    async fn cancel_order(&self, id: OrderId) -> PlatformResult<CancelOutcome> {
        self.run(Endpoint::Cancel, |s, _| s.cancel_order(id))
    }

    async fn cancel_all(&self, tid: &TournamentId, scope: &CancelScope) -> PlatformResult<CancelAllResponse> {
        self.run(Endpoint::CancelAll, |s, _| s.cancel_all(tid, scope))
    }

    async fn orders_page(
        &self,
        tid: &TournamentId,
        status: OrderStatusFilter,
        exchange: Option<&ExchangeId>,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Order>> {
        self.run(Endpoint::Orders, |s, _| {
            s.orders_page(tid, status, exchange, cursor)
        })
    }

    async fn positions(&self, slug: &str) -> PlatformResult<Positions> {
        self.run(Endpoint::Positions, |s, _| s.positions(slug))
    }

    async fn pnl(&self, slug: &str, period: PnlPeriod) -> PlatformResult<Pnl> {
        self.run(Endpoint::Pnl, |s, now| s.pnl(slug, period, now))
    }

    async fn fills_page(&self, slug: &str, cursor: Option<&str>) -> PlatformResult<Page<Fill>> {
        self.run(Endpoint::Fills, |s, _| s.fills_page(slug, cursor))
    }

    async fn leaderboard(
        &self,
        slug: &str,
        period: LeaderboardPeriod,
        limit: u32,
        offset: u32,
    ) -> PlatformResult<Leaderboard> {
        self.run(Endpoint::Leaderboard, |s, _| {
            s.leaderboard(slug, period, limit, offset)
        })
    }

    async fn relationships_page(
        &self,
        tid: &TournamentId,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Relationship>> {
        self.run(Endpoint::Relationships, |s, _| s.relationships_page(tid, cursor))
    }

    async fn constraints(
        &self,
        tid: &TournamentId,
        violations_only: bool,
        min_violation: f64,
    ) -> PlatformResult<Constraints> {
        self.run(Endpoint::Constraints, |s, now| {
            s.constraints(tid, violations_only, min_violation, now)
        })
    }

    async fn realtime_token(&self) -> PlatformResult<RealtimeToken> {
        self.run(Endpoint::RealtimeToken, |s, _| Ok(s.realtime_token.clone()))
    }

    async fn collateral(&self, tid: &TournamentId) -> PlatformResult<Collateral> {
        self.run(Endpoint::Collateral, |s, _| s.collateral(tid))
    }
}
