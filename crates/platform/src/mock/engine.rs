//! Matching and accounting core of [`super::MockPlatform`].
//!
//! One YES-space book per exchange (the mock serves a single tournament), price-time priority,
//! trades at the resting order's price, self-trade prevention, sell canonicalization, and signed
//! positions with cash. Everything is synchronous and `Clone`, so a multi-leg request can execute
//! on a scratch copy and commit atomically.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, SecondsFormat, Utc};
use susq_core::{Action, BookSide, ExchangeId, MarketId, OrderId, Pips, Side, side::yes_space};

use super::{CALLER, MockConfig};
use crate::{
    error::{ApiError, PlatformResult},
    models::*,
};

/// Tolerance for cash comparisons.
const EPS: f64 = 1e-9;

/// Lifecycle of an engine order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Open,
    Filled,
    Cancelled,
    Expired,
    /// The crossing remainder was cancelled by self-trade prevention.
    SelfTradePrevented,
}

/// An order as the engine holds it, in canonical form.
#[derive(Debug, Clone)]
pub(super) struct EngineOrder {
    pub(super) id: i64,
    pub(super) owner: String,
    pub(super) exchange: ExchangeId,
    /// Canonical side, action and side-relative limit price (after sell canonicalization).
    pub(super) side: Side,
    pub(super) action: Action,
    pub(super) price: Pips,
    /// Where the order sits in the YES book, and at what YES price.
    pub(super) book_side: BookSide,
    pub(super) yes_price: Pips,
    pub(super) quantity: i64,
    pub(super) remaining: i64,
    pub(super) status: Status,
    pub(super) created_at: DateTime<Utc>,
    pub(super) expiration: Option<DateTime<Utc>>,
}

impl EngineOrder {
    /// Resting on the book.
    pub(super) fn is_open(&self) -> bool {
        self.status == Status::Open
    }

    /// The `GET /orders` row. `quantity` is the executable (or, once closed, unfilled) remainder.
    pub(super) fn to_wire(&self) -> Order {
        Order {
            id: self.id,
            exchange_id: self.exchange.clone(),
            side: self.side,
            action: self.action,
            quantity: self.remaining as f64,
            price_limit: Some(self.price.as_f64()),
            open: self.is_open(),
            created_at: iso(self.created_at),
            expiration_date: self.expiration.map(iso),
        }
    }

    /// Would a taker on `taker_side` at YES price `taker_px` trade with this resting order?
    fn crossed_by(&self, taker_side: BookSide, taker_px: Pips) -> bool {
        match taker_side {
            BookSide::Bid => self.yes_price <= taker_px,
            BookSide::Ask => self.yes_price >= taker_px,
        }
    }

    /// Price-time priority key: smaller is better on either side of the book.
    fn priority(&self) -> (i64, i64) {
        let p = i64::from(self.yes_price.get());
        match self.book_side {
            BookSide::Bid => (-p, self.id),
            BookSide::Ask => (p, self.id),
        }
    }

    /// The `POST /orders` response for this order after `exec`.
    fn ack(&self, exec: &Execution) -> OrderAck {
        let stp = self.status == Status::SelfTradePrevented;
        OrderAck {
            order_id: Some(self.id),
            exchange_id: self.exchange.clone(),
            open: self.is_open(),
            remaining_quantity: Some(self.remaining as f64),
            action: Some(self.action),
            side: Some(self.side),
            price: Some(self.price.as_f64()),
            quantity: Some(self.quantity),
            terminal_reason_code: stp.then(|| "SelfTradePrevented".to_owned()),
            quantity_traded: exec.traded as f64,
            total_cost: exec.notional,
            fill_price: (exec.traded > 0).then(|| exec.notional / exec.traded as f64),
            all: Some(AllEconomics {
                full_notional_cost: exec.notional,
                effective_entry_cost: exec.cash_out,
                net_buying_power_impact: exec.cash_out,
                ..AllEconomics::default()
            }),
        }
    }
}

/// What a taker order did while matching.
#[derive(Debug, Default)]
struct Execution {
    traded: i64,
    /// Side-relative notional of the fills (the order's `totalCost`).
    notional: f64,
    /// Net cash paid by the taker (negative when netting released cash).
    cash_out: f64,
}

/// A new order before canonicalization.
#[derive(Debug, Clone)]
pub(super) struct Submission {
    pub(super) owner: String,
    pub(super) exchange: ExchangeId,
    pub(super) side: Side,
    pub(super) action: Action,
    pub(super) quantity: i64,
    pub(super) price: Pips,
    pub(super) expiration: Option<DateTime<Utc>>,
}

/// A signed YES position (+YES / -NO) with average-cost accounting.
#[derive(Debug, Clone, Default)]
pub(super) struct Holding {
    pub(super) qty: i64,
    /// Total side-relative cost of the shares held.
    pub(super) cost_basis: f64,
    /// Cash realized by closing shares.
    pub(super) money_earned: f64,
}

impl Holding {
    /// Apply a trade of `yes_delta` YES shares at YES price `p`; returns the cash paid
    /// (negative when cash is received).
    ///
    /// Buying YES first closes held NO (each YES/NO pair pays 1, so a closed NO share yields
    /// `1 - p`); selling YES first closes held YES (yields `p`). The rest opens new shares at the
    /// side-relative price.
    fn apply(&mut self, yes_delta: i64, p: f64) -> f64 {
        let (held_opposite, close_yield, open_cost) = if yes_delta > 0 {
            ((-self.qty).max(0), 1.0 - p, p)
        } else {
            (self.qty.max(0), p, 1.0 - p)
        };
        let shares = yes_delta.abs();
        let closed = shares.min(held_opposite);
        let opened = shares - closed;
        if closed > 0 {
            let held = self.qty.abs();
            self.cost_basis *= (held - closed) as f64 / held as f64;
            self.money_earned += close_yield * closed as f64;
        }
        self.cost_basis += open_cost * opened as f64;
        self.qty += yes_delta;
        open_cost * opened as f64 - close_yield * closed as f64
    }
}

/// One side of a trade, from one owner's point of view.
#[derive(Debug, Clone)]
pub(super) struct FillRecord {
    pub(super) id: i64,
    pub(super) owner: String,
    pub(super) order_id: i64,
    pub(super) exchange: ExchangeId,
    pub(super) yes_price: Pips,
    /// Signed YES change for the owner: `+` bought YES, `-` sold YES / bought NO.
    pub(super) yes_delta: i64,
    pub(super) at: DateTime<Utc>,
}

/// A registered market.
#[derive(Debug, Clone)]
pub(super) struct MarketEntry {
    pub(super) id: MarketId,
    pub(super) title: String,
    pub(super) status: String,
    pub(super) exchanges: Vec<ExchangeId>,
}

/// Where an exchange lives.
#[derive(Debug, Clone)]
pub(super) struct ExchangeInfo {
    pub(super) market_id: MarketId,
    pub(super) market_title: String,
    pub(super) option: String,
}

/// Idempotency store for one write endpoint: key -> (payload fingerprint, stored response).
#[derive(Debug, Clone)]
pub(super) struct Idempotency<T> {
    entries: HashMap<String, (String, T)>,
}

impl<T> Default for Idempotency<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<T: Clone> Idempotency<T> {
    /// The stored response for `key` if the payload matches; `409 CONFLICT` if it does not.
    pub(super) fn replay(&self, key: &str, fingerprint: &str) -> PlatformResult<Option<T>> {
        if key.is_empty() {
            return Err(ApiError::new(400, "VALIDATION_ERROR", "idempotencyKey is required").into());
        }
        match self.entries.get(key) {
            None => Ok(None),
            Some((stored, response)) if stored == fingerprint => Ok(Some(response.clone())),
            Some(_) => {
                Err(ApiError::new(409, "CONFLICT", "idempotency key reused with a different payload").into())
            }
        }
    }

    /// Record the response of a successful execution.
    pub(super) fn remember(&mut self, key: &str, fingerprint: String, response: T) {
        self.entries.insert(key.to_owned(), (fingerprint, response));
    }
}

/// The whole engine state. `Clone` so multi-leg requests can run on a scratch copy.
#[derive(Debug, Clone)]
pub(super) struct State {
    pub(super) cfg: MockConfig,
    pub(super) markets: Vec<MarketEntry>,
    pub(super) exchanges: HashMap<ExchangeId, ExchangeInfo>,
    /// Every order ever created, keyed by id (ids increase with time).
    pub(super) orders: BTreeMap<i64, EngineOrder>,
    pub(super) holdings: BTreeMap<(String, ExchangeId), Holding>,
    pub(super) cash: HashMap<String, f64>,
    pub(super) fills: Vec<FillRecord>,
    pub(super) last_trade: HashMap<ExchangeId, Pips>,
    /// Orders the engine created for [`CALLER`].
    pub(super) caller_orders_created: usize,
    next_order_id: i64,
    next_fill_id: i64,
    pub(super) placed: Idempotency<OrderAck>,
    pub(super) batches: Idempotency<BatchResponse>,
    pub(super) multi_legs: Idempotency<MultiLegResponse>,
    pub(super) relationships: Vec<Relationship>,
    pub(super) constraints: Vec<ConstraintEval>,
    pub(super) leaderboard: Leaderboard,
    pub(super) realtime_token: RealtimeToken,
    pub(super) collateral: Collateral,
}

impl State {
    /// Empty engine; the caller starts with `cfg.starting_balance`.
    pub(super) fn new(cfg: MockConfig) -> Self {
        let cash = HashMap::from([(CALLER.to_owned(), cfg.starting_balance)]);
        Self {
            cfg,
            markets: Vec::new(),
            exchanges: HashMap::new(),
            orders: BTreeMap::new(),
            holdings: BTreeMap::new(),
            cash,
            fills: Vec::new(),
            last_trade: HashMap::new(),
            caller_orders_created: 0,
            next_order_id: 1,
            next_fill_id: 1,
            placed: Idempotency::default(),
            batches: Idempotency::default(),
            multi_legs: Idempotency::default(),
            relationships: Vec::new(),
            constraints: Vec::new(),
            leaderboard: Leaderboard::default(),
            realtime_token: RealtimeToken::default(),
            collateral: Collateral::default(),
        }
    }

    /// Register an open market and its exchanges.
    pub(super) fn add_market(&mut self, id: MarketId, title: &str, exchanges: &[(ExchangeId, String)]) {
        for (exchange, option) in exchanges {
            let info = ExchangeInfo {
                market_id: id.clone(),
                market_title: title.to_owned(),
                option: option.clone(),
            };
            self.exchanges.insert(exchange.clone(), info);
        }
        self.markets.push(MarketEntry {
            id,
            title: title.to_owned(),
            status: "open".to_owned(),
            exchanges: exchanges.iter().map(|(e, _)| e.clone()).collect(),
        });
    }

    /// Signed YES position of `owner` on `exchange`.
    pub(super) fn position(&self, owner: &str, exchange: &ExchangeId) -> i64 {
        self.holdings
            .get(&(owner.to_owned(), exchange.clone()))
            .map_or(0, |h| h.qty)
    }

    /// Cash of `owner` (other owners start at 0 and may go negative).
    pub(super) fn cash_of(&self, owner: &str) -> f64 {
        self.cash.get(owner).copied().unwrap_or(0.0)
    }

    /// Orders of `owner`, oldest first.
    pub(super) fn orders_of<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = &'a EngineOrder> {
        self.orders.values().filter(move |o| o.owner == owner)
    }

    /// Lazily expire every resting order whose expiration is at or before `now`.
    pub(super) fn expire(&mut self, now: DateTime<Utc>) {
        for order in self.orders.values_mut() {
            if order.is_open() && order.expiration.is_some_and(|t| t <= now) {
                order.status = Status::Expired;
            }
        }
    }

    /// `400 INSUFFICIENT_BALANCE` unless the owner's unreserved cash covers the order's worst case.
    pub(super) fn check_balance(&self, sub: &Submission) -> PlatformResult<()> {
        let (side, action, price) = self.canonicalize(sub);
        let required = self.required_cash(&sub.owner, &sub.exchange, side, action, sub.quantity, price);
        let available = self.available_cash(&sub.owner);
        if required > available + EPS {
            return Err(ApiError::new(
                400,
                "INSUFFICIENT_BALANCE",
                &format!("order needs {required:.4}, available {available:.4}"),
            )
            .into());
        }
        Ok(())
    }

    /// Canonicalize, match, then rest or close one order. No balance check: see [`Self::check_balance`].
    pub(super) fn submit(&mut self, sub: Submission, now: DateTime<Utc>) -> (OrderId, OrderAck) {
        let (side, action, price) = self.canonicalize(&sub);
        let (book_side, yes_price) = yes_space(side, action, price);
        let id = self.next_order_id;
        self.next_order_id += 1;
        if sub.owner == CALLER {
            self.caller_orders_created += 1;
        }
        let mut order = EngineOrder {
            id,
            owner: sub.owner,
            exchange: sub.exchange,
            side,
            action,
            price,
            book_side,
            yes_price,
            quantity: sub.quantity,
            remaining: sub.quantity,
            status: Status::Open,
            created_at: now,
            expiration: sub.expiration,
        };
        let exec = self.match_incoming(&mut order, now);
        order.status = if order.remaining == 0 {
            Status::Filled
        } else if self.would_self_trade(&order) {
            Status::SelfTradePrevented
        } else {
            Status::Open
        };
        let ack = order.ack(&exec);
        self.orders.insert(id, order);
        (OrderId(id), ack)
    }

    /// `DELETE /orders/{id}` on behalf of [`CALLER`].
    pub(super) fn cancel(&mut self, id: OrderId) -> PlatformResult<CancelOutcome> {
        let Some(order) = self.orders.get_mut(&id.0) else {
            return Err(ApiError::new(404, "NOT_FOUND", &format!("order {id} not found")).into());
        };
        if order.owner != CALLER {
            return Err(
                ApiError::new(403, "FORBIDDEN", &format!("order {id} belongs to another user")).into(),
            );
        }
        if !order.is_open() {
            return Ok(CancelOutcome::AlreadyClosed);
        }
        order.status = Status::Cancelled;
        Ok(CancelOutcome::Cancelled)
    }

    /// Cancel every open order of `owner` inside `scope`; returns how many were cancelled.
    pub(super) fn cancel_open(&mut self, owner: &str, scope: impl Fn(&EngineOrder) -> bool) -> i64 {
        let mut cancelled = 0;
        for order in self.orders.values_mut() {
            if order.owner == owner && order.is_open() && scope(order) {
                order.status = Status::Cancelled;
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Best resting YES price on one side of an exchange's book.
    pub(super) fn best(&self, exchange: &ExchangeId, side: BookSide) -> Option<Pips> {
        self.resting(exchange, side)
            .min_by_key(|o| o.priority())
            .map(|o| o.yes_price)
    }

    /// Aggregated price levels, best first, at most `depth` of them.
    pub(super) fn levels(&self, exchange: &ExchangeId, side: BookSide, depth: usize) -> Vec<BookLevel> {
        let mut by_price: BTreeMap<u32, i64> = BTreeMap::new();
        for order in self.resting(exchange, side) {
            *by_price.entry(order.yes_price.get()).or_default() += order.remaining;
        }
        let level = |(raw, qty): (u32, i64)| BookLevel {
            price: f64::from(raw) / f64::from(susq_core::price::PIPS_PER_UNIT),
            quantity: qty as f64,
        };
        match side {
            BookSide::Bid => by_price.into_iter().rev().take(depth).map(level).collect(),
            BookSide::Ask => by_price.into_iter().take(depth).map(level).collect(),
        }
    }

    /// YES mark price: last trade, else the book mid, else unknown.
    pub(super) fn mark_yes(&self, exchange: &ExchangeId) -> Option<f64> {
        if let Some(last) = self.last_trade.get(exchange) {
            return Some(last.as_f64());
        }
        let bid = self.best(exchange, BookSide::Bid)?;
        let ask = self.best(exchange, BookSide::Ask)?;
        Some((bid.as_f64() + ask.as_f64()) / 2.0)
    }

    /// Resting orders on one side of an exchange's book, in id order.
    fn resting<'a>(
        &'a self,
        exchange: &'a ExchangeId,
        side: BookSide,
    ) -> impl Iterator<Item = &'a EngineOrder> {
        self.orders
            .values()
            .filter(move |o| o.is_open() && o.book_side == side && &o.exchange == exchange)
    }

    /// Sell canonicalization: a sell not fully backed by the owner's uncommitted holding on that
    /// side becomes the complement buy (`sell yes q@p` == `buy no q@(1-p)`).
    fn canonicalize(&self, sub: &Submission) -> (Side, Action, Pips) {
        let backed =
            sub.action == Action::Buy || self.sellable(&sub.owner, &sub.exchange, sub.side) >= sub.quantity;
        if backed {
            (sub.side, sub.action, sub.price)
        } else {
            (sub.side.opposite(), Action::Buy, sub.price.complement())
        }
    }

    /// Shares of `side` the owner holds and has not already offered in resting conventional sells.
    fn sellable(&self, owner: &str, exchange: &ExchangeId, side: Side) -> i64 {
        let held = (self.position(owner, exchange) * side.sign()).max(0);
        let committed: i64 = self
            .orders_of(owner)
            .filter(|o| o.is_open() && &o.exchange == exchange && o.action == Action::Sell && o.side == side)
            .map(|o| o.remaining)
            .sum();
        held - committed
    }

    /// Worst-case cash a canonical order needs: buys pay for the shares they open (shares that
    /// net against the opposite holding pay out instead); conventional sells need nothing.
    fn required_cash(
        &self,
        owner: &str,
        exchange: &ExchangeId,
        side: Side,
        action: Action,
        quantity: i64,
        price: Pips,
    ) -> f64 {
        if action == Action::Sell {
            return 0.0;
        }
        let opposite_held = (self.position(owner, exchange) * side.opposite().sign()).max(0);
        let opening = quantity - quantity.min(opposite_held);
        opening as f64 * price.as_f64()
    }

    /// Cash not reserved by the owner's resting buys.
    fn available_cash(&self, owner: &str) -> f64 {
        let reserved: f64 = self
            .orders_of(owner)
            .filter(|o| o.is_open() && o.action == Action::Buy)
            .map(|o| o.remaining as f64 * o.price.as_f64())
            .sum();
        self.cash_of(owner) - reserved
    }

    /// Fill `taker` against other owners' crossing liquidity, best price then oldest first.
    fn match_incoming(&mut self, taker: &mut EngineOrder, now: DateTime<Utc>) -> Execution {
        let mut exec = Execution::default();
        while taker.remaining > 0 {
            let Some(maker_id) = self.best_counter(taker) else {
                break;
            };
            let Some(maker) = self.orders.get_mut(&maker_id) else {
                break;
            };
            let qty = taker.remaining.min(maker.remaining);
            let px = maker.yes_price;
            maker.remaining -= qty;
            if maker.remaining == 0 {
                maker.status = Status::Filled;
            }
            let maker_owner = maker.owner.clone();
            taker.remaining -= qty;
            let yes_delta = taker.book_side.yes_delta_sign() * qty;
            exec.cash_out += self.settle(&taker.owner, taker.id, &taker.exchange, yes_delta, px, now);
            self.settle(&maker_owner, maker_id, &taker.exchange, -yes_delta, px, now);
            self.last_trade.insert(taker.exchange.clone(), px);
            exec.traded += qty;
            exec.notional += qty as f64 * side_price(taker.side, px.as_f64());
        }
        exec
    }

    /// Best resting order of another owner that `taker` crosses.
    fn best_counter(&self, taker: &EngineOrder) -> Option<i64> {
        self.resting(&taker.exchange, taker.book_side.opposite())
            .filter(|m| m.owner != taker.owner && m.crossed_by(taker.book_side, taker.yes_price))
            .min_by_key(|m| m.priority())
            .map(|m| m.id)
    }

    /// Does the owner have a resting order the remainder of `order` would cross?
    fn would_self_trade(&self, order: &EngineOrder) -> bool {
        self.resting(&order.exchange, order.book_side.opposite())
            .any(|m| m.owner == order.owner && m.crossed_by(order.book_side, order.yes_price))
    }

    /// Book one owner's side of a trade: position, cash and fill record. Returns cash paid.
    fn settle(
        &mut self,
        owner: &str,
        order_id: i64,
        exchange: &ExchangeId,
        yes_delta: i64,
        px: Pips,
        now: DateTime<Utc>,
    ) -> f64 {
        let paid = self
            .holdings
            .entry((owner.to_owned(), exchange.clone()))
            .or_default()
            .apply(yes_delta, px.as_f64());
        *self.cash.entry(owner.to_owned()).or_insert(0.0) -= paid;
        self.fills.push(FillRecord {
            id: self.next_fill_id,
            owner: owner.to_owned(),
            order_id,
            exchange: exchange.clone(),
            yes_price: px,
            yes_delta,
            at: now,
        });
        self.next_fill_id += 1;
        paid
    }
}

/// A YES price seen from `side` (`1 - p` for NO).
pub(super) fn side_price(side: Side, yes: f64) -> f64 {
    match side {
        Side::Yes => yes,
        Side::No => 1.0 - yes,
    }
}

/// Wire timestamp.
pub(super) fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}
