//! Wire models for the endpoints in `docs/API.md`, shaped after the official OpenAPI document.
//!
//! Every response type is tolerant: unknown fields are ignored and missing fields default, so a
//! platform addition never breaks decoding. Request types are strict and use core types
//! ([`Pips`], [`Side`], [`Action`]) so an invalid order cannot be expressed.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use susq_core::{Action, ExchangeId, MarketId, OrderId, Pips, Qty, Side, TournamentId};

// ───────────────────────────── pagination ─────────────────────────────

/// Cursor pagination block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Pagination {
    pub total: Option<i64>,
    pub limit: Option<i64>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

/// One page of `data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", bound(deserialize = "T: Deserialize<'de>"))]
pub struct Page<T> {
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    #[serde(default)]
    pub pagination: Pagination,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            pagination: Pagination::default(),
        }
    }
}

// ───────────────────────────── tournaments & markets ─────────────────────────────

/// `GET /tournaments/{slug}`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Tournament {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub status: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub initial_balance: f64,
    pub currency_name: String,
    pub member_count: u32,
    pub market_count: u32,
    pub my_balance: f64,
    pub is_pending_enrolment: bool,
}

/// Market lifecycle filter for listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketStatusFilter {
    Open,
    Closed,
    Settled,
    Any,
}

impl MarketStatusFilter {
    /// Query-string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Settled => "settled",
            Self::Any => "any",
        }
    }
}

/// Exchange summary inside a market row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketExchange {
    pub id: ExchangeId,
    #[serde(default)]
    pub option: Option<String>,
    /// Tournament latest trade price; null until a tournament trade happens.
    #[serde(default)]
    pub latest_price: Option<f64>,
    #[serde(default)]
    pub initial_price: Option<f64>,
}

/// A row of `GET /tournaments/{slug}/markets`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    pub id: MarketId,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub settlement_date: Option<String>,
    #[serde(default)]
    pub settled_with: Option<String>,
    #[serde(default)]
    pub is_composite: bool,
    #[serde(default)]
    pub is_multi_outcome: bool,
    #[serde(default)]
    pub exchanges: Vec<MarketExchange>,
}

// ───────────────────────────── prices & books ─────────────────────────────

/// Top of book for one exchange (`GET /exchanges/prices`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceQuote {
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub market_id: Option<MarketId>,
    #[serde(default)]
    pub option: Option<String>,
    #[serde(default)]
    pub latest_price: Option<f64>,
    #[serde(default)]
    pub best_bid: Option<f64>,
    #[serde(default)]
    pub best_ask: Option<f64>,
    #[serde(default)]
    pub spread: Option<f64>,
}

/// `GET /exchanges/prices` response (request order preserved).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BulkPrices {
    pub data: Vec<PriceQuote>,
    pub missing_ids: Vec<ExchangeId>,
}

/// One price rung.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: f64,
    pub quantity: f64,
}

/// `GET /exchanges/{id}/orderbook`: bids descending, asks ascending, YES space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBook {
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub market_id: Option<MarketId>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub bids: Vec<BookLevel>,
    #[serde(default)]
    pub asks: Vec<BookLevel>,
    #[serde(default)]
    pub best_bid: Option<f64>,
    #[serde(default)]
    pub best_ask: Option<f64>,
    #[serde(default)]
    pub spread: Option<f64>,
}

// ───────────────────────────── orders (requests) ─────────────────────────────

fn pips_as_decimal<S: Serializer>(p: &Pips, s: S) -> Result<S::Ok, S::Error> {
    // serde_json prints the shortest round-trip form, which is the exact decimal (tested in core).
    s.serialize_f64(p.as_f64())
}

fn pips_from_decimal<'de, D: Deserializer<'de>>(d: D) -> Result<Pips, D::Error> {
    let x = f64::deserialize(d)?;
    Pips::from_f64(x).map_err(serde::de::Error::custom)
}

fn expiration_to_iso<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&t.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// One limit order as sent in `POST /orders`, batch `orders[]` and multi-leg `legs[]`.
/// We never send market orders: `price` and `expirationDate` are always present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequest {
    pub exchange_id: ExchangeId,
    pub side: Side,
    pub action: Action,
    pub quantity: i64,
    #[serde(serialize_with = "pips_as_decimal", deserialize_with = "pips_from_decimal")]
    pub price: Pips,
    #[serde(serialize_with = "expiration_to_iso")]
    pub expiration_date: DateTime<Utc>,
    pub tournament_id: TournamentId,
}

impl OrderRequest {
    /// Build a limit order.
    pub fn limit(
        exchange_id: ExchangeId,
        side: Side,
        action: Action,
        quantity: Qty,
        price: Pips,
        expiration_date: DateTime<Utc>,
        tournament_id: TournamentId,
    ) -> Self {
        Self {
            exchange_id,
            side,
            action,
            quantity: quantity.get(),
            price,
            expiration_date,
            tournament_id,
        }
    }
}

/// `POST /orders` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderRequest {
    #[serde(flatten)]
    pub order: OrderRequest,
    pub idempotency_key: String,
}

/// `POST /orders/batch` body (≤ 50 orders under one key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRequest {
    pub idempotency_key: String,
    pub orders: Vec<OrderRequest>,
}

/// `POST /orders/multi-leg` body (≤ 10 legs, all or nothing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiLegRequest {
    pub legs: Vec<OrderRequest>,
    pub idempotency_key: String,
    /// Never set for arbitrage legs (it would 422 them); present for completeness of the contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationship_constraint: Option<String>,
}

/// Max orders in one batch.
pub const MAX_BATCH: usize = 50;
/// Max legs in one multi-leg order.
pub const MAX_LEGS: usize = 10;
/// Max ids in one bulk price request.
pub const MAX_PRICE_IDS: usize = 100;

// ───────────────────────────── orders (responses) ─────────────────────────────

/// Authoritative ALL collateral economics of an execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AllEconomics {
    pub full_notional_cost: f64,
    pub effective_entry_cost: f64,
    pub collateral_savings: f64,
    pub relationship_ids: Vec<String>,
    pub component_id: Option<String>,
    pub guaranteed_payout_floor_after: f64,
    pub outstanding_advance_after: f64,
    pub collateral_repayment: f64,
    pub net_buying_power_impact: f64,
}

/// `POST /orders` 200 (also each successful batch item / multi-leg leg).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderAck {
    #[serde(default)]
    pub order_id: Option<i64>,
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub remaining_quantity: Option<f64>,
    /// Canonical engine action (a sell may be canonicalized to the complement buy).
    #[serde(default)]
    pub action: Option<Action>,
    #[serde(default)]
    pub side: Option<Side>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub quantity: Option<i64>,
    #[serde(default)]
    pub terminal_reason_code: Option<String>,
    #[serde(default)]
    pub quantity_traded: f64,
    #[serde(default)]
    pub total_cost: f64,
    #[serde(default)]
    pub fill_price: Option<f64>,
    #[serde(default)]
    pub all: Option<AllEconomics>,
}

impl OrderAck {
    /// Platform order id, if one was assigned.
    pub fn id(&self) -> Option<OrderId> {
        self.order_id.map(OrderId)
    }
}

/// One item of a batch response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItem {
    pub index: usize,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub status: u16,
    /// An [`OrderAck`] when `ok`, otherwise an error object.
    #[serde(default)]
    pub data: Value,
}

impl BatchItem {
    /// Decode the ack of a successful item.
    pub fn ack(&self) -> Option<OrderAck> {
        if !self.ok {
            return None;
        }
        serde_json::from_value(self.data.clone()).ok()
    }

    /// Error code of a failed item (`data.error.code` or `data.code`).
    pub fn error_code(&self) -> Option<String> {
        self.data
            .pointer("/error/code")
            .or_else(|| self.data.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

/// `POST /orders/batch` 200/207/422.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResponse {
    /// HTTP status the envelope came with (200, 207 or 422); not part of the body.
    #[serde(skip)]
    pub http_status: u16,
    #[serde(default)]
    pub results: Vec<BatchItem>,
}

/// One leg of a multi-leg 200.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiLegItem {
    pub index: usize,
    pub data: OrderAck,
}

/// `POST /orders/multi-leg` 200.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MultiLegResponse {
    #[serde(default)]
    pub results: Vec<MultiLegItem>,
}

/// Result of `DELETE /orders/{id}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 200: cancelled now.
    Cancelled,
    /// 409: already filled or cancelled (fine).
    AlreadyClosed,
}

/// Scope of `POST /orders/cancel-all` (always within one tournament).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CancelScope {
    pub exchange_id: Option<ExchangeId>,
    pub market_id: Option<MarketId>,
}

/// One per-order failure inside a cancel-all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelError {
    pub order_id: i64,
    #[serde(default)]
    pub error: String,
}

/// `POST /orders/cancel-all` 200/207/422.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAllResponse {
    /// 200 = all cancelled; 207/422 = confirm with open orders before re-quoting.
    #[serde(skip)]
    pub http_status: u16,
    #[serde(default)]
    pub cancelled: i64,
    #[serde(default)]
    pub errors: Vec<CancelError>,
}

/// Order status filter for `GET /orders`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatusFilter {
    Open,
    Closed,
    Expired,
    All,
}

impl OrderStatusFilter {
    /// Query-string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Expired => "expired",
            Self::All => "all",
        }
    }
}

/// A row of `GET /orders`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub id: i64,
    pub exchange_id: ExchangeId,
    pub side: Side,
    pub action: Action,
    /// Executable remainder while open.
    #[serde(default)]
    pub quantity: f64,
    #[serde(default)]
    pub price_limit: Option<f64>,
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub expiration_date: Option<String>,
}

// ───────────────────────────── portfolio ─────────────────────────────

/// A FIFO lot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Lot {
    pub lot_id: String,
    pub side: String,
    pub quantity: f64,
    pub entry_price: f64,
    pub opened_at: String,
}

/// One position (`quantity` > 0 = YES, < 0 = NO).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub market_id: Option<MarketId>,
    #[serde(default)]
    pub market_title: String,
    #[serde(default)]
    pub option: Option<String>,
    #[serde(default)]
    pub settled: bool,
    #[serde(default)]
    pub quantity: f64,
    #[serde(default)]
    pub avg_cost: f64,
    #[serde(default)]
    pub current_price: Option<f64>,
    #[serde(default)]
    pub market_value: f64,
    #[serde(default)]
    pub cost_basis: f64,
    #[serde(default)]
    pub unrealized_pnl: f64,
    #[serde(default)]
    pub money_earned: f64,
    #[serde(default)]
    pub lots: Vec<Lot>,
}

/// Portfolio summary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PositionsSummary {
    pub total_market_value: f64,
    pub total_cost_basis: f64,
    pub total_unrealized_pnl: f64,
}

/// `GET /tournaments/{slug}/portfolio/positions`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Positions {
    pub positions: Vec<Position>,
    pub summary: PositionsSummary,
}

/// P&L lookback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnlPeriod {
    Day,
    Week,
    Month,
    Quarter,
    Year,
    All,
}

impl PnlPeriod {
    /// Query-string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
            Self::All => "all",
        }
    }
}

/// `GET /tournaments/{slug}/portfolio/pnl`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Pnl {
    pub period: String,
    pub period_start: Option<String>,
    pub period_end: String,
    pub period_pnl: Option<f64>,
    pub unrealized_pnl: f64,
    /// What the leaderboard ranks on.
    pub total_account_value: f64,
    pub total_holdings_value: f64,
    pub total_cost_basis: f64,
    pub roi: Option<f64>,
    pub sharpe: Option<f64>,
}

/// A fill (`quantity` is outcome-signed: negative = NO).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub id: i64,
    #[serde(default)]
    pub order_id: Option<i64>,
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub market_id: Option<MarketId>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub quantity: f64,
    pub side: Side,
    #[serde(default)]
    pub filled_at: String,
}

/// Leaderboard period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderboardPeriod {
    Day,
    Week,
    Month,
    All,
}

impl LeaderboardPeriod {
    /// Query-string form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "1d",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::All => "all",
        }
    }
}

/// A leaderboard row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LeaderboardEntry {
    pub rank: i64,
    pub profile_id: String,
    pub username: Option<String>,
    pub pnl: f64,
    pub trades_count: i64,
    pub volume: f64,
    pub win_rate: Option<f64>,
    pub roi: f64,
}

/// `GET /tournaments/{slug}/leaderboard`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Leaderboard {
    pub leaderboard: Vec<LeaderboardEntry>,
    /// Ranked field size.
    pub total: i64,
    pub period: String,
    pub limit: i64,
    pub offset: i64,
    pub my_rank: Option<i64>,
}

/// One ALL collateral component.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CollateralComponent {
    pub profile_id: String,
    pub tournament_id: Option<String>,
    pub outstanding_advance: f64,
    pub component_id: String,
    pub covered_exchange_ids: Vec<ExchangeId>,
    pub relationship_ids: Vec<String>,
    pub guaranteed_payout_floor: Option<f64>,
    pub diagnostic: Option<String>,
}

/// `GET /portfolio/collateral`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Collateral {
    pub data: Vec<CollateralComponent>,
}

// ───────────────────────────── relationships ─────────────────────────────

/// Canonical ALL relationship type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipType {
    Monotonic,
    Complementary,
    MutuallyExclusive,
    Implication,
    BooleanAnd,
    BooleanOr,
    BooleanNot,
    ConditionalIf,
    BooleanExpression,
    /// A type added after this build; never traded.
    #[serde(other)]
    Unknown,
}

/// An exchange's logical role in a relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Member,
    Antecedent,
    Consequent,
    Output,
    Input,
    #[serde(other)]
    Unknown,
}

/// A labeled exchange node of a relationship.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipNode {
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub market_id: Option<MarketId>,
    #[serde(default)]
    pub market_title: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub current_price: Option<f64>,
    pub role: NodeRole,
}

/// Direction metadata.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Direction {
    pub kind: String,
    pub from_exchange_ids: Vec<ExchangeId>,
    pub to_exchange_ids: Vec<ExchangeId>,
    pub description: String,
}

/// Human-readable rule text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConstraintText {
    pub description: String,
    pub price_rule: String,
    pub bound_semantics: String,
}

/// A row of `GET /relationships`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relationship {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: RelationshipType,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Members exhaust every valid outcome of their component.
    #[serde(default)]
    pub is_exhaustive: bool,
    #[serde(default)]
    pub nodes: Vec<RelationshipNode>,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub constraint: Option<ConstraintText>,
    #[serde(default)]
    pub version: i64,
}

/// A suggested corrective trade (no size).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectiveTrade {
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub outcome_side: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub market_id: Option<MarketId>,
    #[serde(default)]
    pub current_price: Option<f64>,
}

/// A price the engine observed when evaluating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedPrice {
    pub exchange_id: ExchangeId,
    #[serde(default)]
    pub price: f64,
    #[serde(default)]
    pub market_id: Option<MarketId>,
}

/// One engine evaluation (`GET /relationships/constraints` row, also the realtime `violation` payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConstraintEval {
    pub relationship_id: String,
    #[serde(rename = "type")]
    pub kind: RelationshipType,
    #[serde(default)]
    pub violation_amount: f64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub suggested_corrective_trades: Vec<CorrectiveTrade>,
    #[serde(default)]
    pub observed_prices: Option<Vec<ObservedPrice>>,
    #[serde(default)]
    pub bound: Option<f64>,
    #[serde(default)]
    pub lower_bound: Option<f64>,
    #[serde(default)]
    pub upper_bound: Option<f64>,
    #[serde(default)]
    pub current_value: Option<f64>,
    /// `satisfied` | `violated` | `unevaluable`.
    #[serde(default)]
    pub evaluation_status: String,
    #[serde(default)]
    pub diagnostic: Option<String>,
    #[serde(default)]
    pub tournament_id: Option<String>,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub computed_at: Option<String>,
}

/// `GET /relationships/constraints`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Constraints {
    pub data: Vec<ConstraintEval>,
    pub violations_count: i64,
    pub computed_at: String,
}

// ───────────────────────────── realtime ─────────────────────────────

/// Channels block of the token response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeChannels {
    /// `user:{profile_id}`
    pub user: String,
}

/// `POST /realtime/token`
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RealtimeToken {
    pub token: String,
    pub expires_at: String,
    pub supabase_url: String,
    pub anon_key: String,
    pub channels: RealtimeChannels,
}

impl std::fmt::Debug for RealtimeToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeToken")
            .field("expires_at", &self.expires_at)
            .field("supabase_url", &self.supabase_url)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

impl RealtimeToken {
    /// Parsed expiry, if well-formed.
    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.expires_at)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("ts")
    }

    #[test]
    fn place_order_body_matches_the_api() {
        let req = PlaceOrderRequest {
            order: OrderRequest::limit(
                ExchangeId::new("381"),
                Side::No,
                Action::Buy,
                Qty(37),
                Pips::new(1450).expect("valid"),
                ts("2026-10-09T00:00:30Z"),
                TournamentId::new("t-uuid"),
            ),
            idempotency_key: "k".into(),
        };
        let json = serde_json::to_string(&req).expect("json");
        assert_eq!(
            json,
            r#"{"exchangeId":"381","side":"no","action":"buy","quantity":37,"price":0.145,"expirationDate":"2026-10-09T00:00:30Z","tournamentId":"t-uuid","idempotencyKey":"k"}"#
        );
        let back: PlaceOrderRequest = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, req);
    }

    #[test]
    fn responses_tolerate_extra_and_missing_fields() {
        let ack: OrderAck = serde_json::from_str(
            r#"{"orderId":9,"exchangeId":"381","open":true,"quantityTraded":0,"totalCost":0,
                "fillPrice":null,"all":null,"futureField":{"x":1}}"#,
        )
        .expect("ack");
        assert_eq!(ack.id(), Some(OrderId(9)));
        let t: Tournament =
            serde_json::from_str(r#"{"id":"u","memberCount":1200,"newThing":true}"#).expect("t");
        assert_eq!(t.member_count, 1200);
        let r: Relationship = serde_json::from_str(
            r#"{"id":"r","type":"quantum_link","nodes":[{"exchangeId":"1","role":"member"}]}"#,
        )
        .expect("rel");
        assert_eq!(r.kind, RelationshipType::Unknown);
    }

    #[test]
    fn batch_item_helpers() {
        let ok: BatchItem = serde_json::from_str(
            r#"{"index":0,"ok":true,"status":200,"data":{"orderId":1,"exchangeId":"5","open":false,"quantityTraded":3,"totalCost":1.2}}"#,
        )
        .expect("item");
        assert_eq!(ok.ack().and_then(|a| a.order_id), Some(1));
        let rl: BatchItem =
            serde_json::from_str(r#"{"index":1,"ok":false,"status":429,"data":{"code":"RATE_LIMITED"}}"#)
                .expect("item");
        assert_eq!(rl.error_code().as_deref(), Some("RATE_LIMITED"));
        let tn: BatchItem = serde_json::from_str(
            r#"{"index":2,"ok":false,"status":403,"data":{"error":{"code":"TERMS_NOT_ACKNOWLEDGED"}}}"#,
        )
        .expect("item");
        assert_eq!(tn.error_code().as_deref(), Some("TERMS_NOT_ACKNOWLEDGED"));
    }

    #[test]
    fn off_tick_request_price_is_rejected_on_parse() {
        let bad = r#"{"exchangeId":"1","side":"yes","action":"buy","quantity":1,"price":0.4551,
                      "expirationDate":"2026-10-09T00:00:30Z","tournamentId":"t"}"#;
        assert!(serde_json::from_str::<OrderRequest>(bad).is_err());
    }

    #[test]
    fn realtime_token_debug_hides_the_token() {
        let t = RealtimeToken {
            token: "jwt-secret-value".into(),
            ..Default::default()
        };
        assert!(!format!("{t:?}").contains("jwt-secret-value"));
    }
}
