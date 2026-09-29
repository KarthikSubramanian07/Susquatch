"""Tolerant pydantic models for every platform response Susquatch reads, plus the order request type.

Rules for this module:

* Field names are snake_case in Python and lowerCamelCase on the wire (``alias_generator``).
* ``extra="allow"``: an unknown new field never breaks us, and stays reachable via ``model_extra``.
* Anything the docs mark as nullable is ``| None``; fields we don't strictly need default to ``None``
  so a partial response (older replayed batch items, for instance) still parses.
* The docs declare quantities as ``number`` in most read models, so they are floats here; convert
  with :func:`as_qty` where integer shares are required.
* Prices stay decimal floats in these models (wire representation). Convert to pips with
  :func:`susq.platform.prices.to_pips` at the boundary.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field
from pydantic.alias_generators import to_camel

from susq.platform.prices import to_price, validate_limit

__all__ = [
    "Account",
    "AllEconomics",
    "BatchItemResult",
    "BatchResult",
    "BookLevel",
    "BulkPrices",
    "CancelAllError",
    "CancelAllResult",
    "CancelResult",
    "Candle",
    "Collateral",
    "CollateralComponent",
    "ConstraintEvaluation",
    "ConstraintSemantics",
    "ConstraintsResult",
    "CorrectiveTrade",
    "Fill",
    "FillsPage",
    "Leaderboard",
    "LeaderboardEntry",
    "Market",
    "MarketExchange",
    "MarketNodes",
    "MarketsPage",
    "MultiLegItem",
    "MultiLegResult",
    "ObservedPrice",
    "Order",
    "OrderBook",
    "OrderFill",
    "OrderFills",
    "OrderRequest",
    "OrdersPage",
    "Pagination",
    "PlaceOrderResult",
    "PnL",
    "Position",
    "PositionLot",
    "Positions",
    "PositionsSummary",
    "PriceHistory",
    "PriceSnapshot",
    "RealtimeToken",
    "Relationship",
    "RelationshipDirection",
    "RelationshipNode",
    "RelationshipsPage",
    "Settlement",
    "SettlementsPage",
    "Tournament",
    "TournamentsPage",
    "Trade",
    "TradesPage",
    "as_qty",
    "iso_utc",
]


class ApiModel(BaseModel):
    model_config = ConfigDict(
        extra="allow",
        alias_generator=to_camel,
        populate_by_name=True,
        frozen=False,
    )


def as_qty(value: float | None) -> int:
    """Integer share count from a ``number`` field (the engine holds int32 quantities)."""
    return 0 if value is None else round(value)


def iso_utc(ts: datetime) -> str:
    """ISO-8601 UTC with a trailing Z and second precision (what we send as ``expirationDate``)."""
    if ts.tzinfo is None:
        raise ValueError("naive datetime")
    return ts.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


# ── pagination ──────────────────────────────────────────────────────────────────────────────


class Pagination(ApiModel):
    limit: int | None = None
    has_more: bool = False
    next_cursor: str | None = None
    total: int | None = None
    offset: int | None = None


# ── account & tournaments ───────────────────────────────────────────────────────────────────


class Account(ApiModel):
    id: str
    username: str | None = None
    balance: float | None = None


class Tournament(ApiModel):
    id: str
    slug: str
    name: str | None = None
    status: str | None = None
    start_date: datetime | None = None
    end_date: datetime | None = None
    initial_balance: float | None = None
    currency_name: str | None = None
    member_count: int | None = None
    market_count: int | None = None
    my_balance: float | None = None
    is_pending_enrolment: bool | None = None


class TournamentsPage(ApiModel):
    data: list[Tournament]
    pagination: Pagination = Field(default_factory=Pagination)


# ── markets & books ─────────────────────────────────────────────────────────────────────────


class BookLevel(ApiModel):
    price: float
    quantity: float


class MarketExchange(ApiModel):
    id: str
    option: str | None = None
    latest_price: float | None = None
    initial_price: float | None = None
    # Only on GET /tournaments/{slug}/markets/{id} (embedded tournament book):
    bids: list[BookLevel] | None = None
    asks: list[BookLevel] | None = None
    best_bid: float | None = None
    best_ask: float | None = None


class Market(ApiModel):
    id: str
    title: str
    status: str | None = None
    created_at: datetime | None = None
    settlement_date: datetime | None = None
    settled_with: str | None = None
    settled_on: datetime | None = None
    categories: list[Any] = Field(default_factory=list)
    is_composite: bool | None = None
    is_multi_outcome: bool | None = None
    exchanges: list[MarketExchange] = Field(default_factory=list)
    overround: float | None = None
    has_arbitrage_opportunity: bool | None = None


class MarketsPage(ApiModel):
    data: list[Market]
    pagination: Pagination = Field(default_factory=Pagination)


class MarketNodes(ApiModel):
    market_id: str = Field(alias="market_id")
    root: dict[str, Any]
    contexts: list[dict[str, Any]] = Field(default_factory=list)


class PriceSnapshot(ApiModel):
    exchange_id: str
    market_id: str | None = None
    option: str | None = None
    latest_price: float | None = None
    best_bid: float | None = None
    best_ask: float | None = None
    spread: float | None = None


class BulkPrices(ApiModel):
    data: list[PriceSnapshot]
    missing_ids: list[str] = Field(default_factory=list)


class OrderBook(ApiModel):
    """YES-perspective book: bids descending, asks ascending."""

    exchange_id: str
    market_id: str | None = None
    depth: int | None = None
    bids: list[BookLevel] = Field(default_factory=list)
    asks: list[BookLevel] = Field(default_factory=list)
    best_bid: float | None = None
    best_ask: float | None = None
    spread: float | None = None


class Trade(ApiModel):
    id: str
    created_at: datetime | None = None
    price: float | None = None
    size: int
    side: str | None = None
    volume: float | None = None


class TradesPage(ApiModel):
    exchange_id: str
    market_id: str | None = None
    data: list[Trade]
    pagination: Pagination = Field(default_factory=Pagination)


class Candle(ApiModel):
    time: datetime
    open: float | None = None
    high: float | None = None
    low: float | None = None
    close: float | None = None
    vwap: float | None = None
    volume: float = 0.0
    trade_count: int = 0


class PriceHistory(ApiModel):
    exchange_id: str
    market_id: str | None = None
    resolution: str
    candles: list[Candle] = Field(default_factory=list)


# ── orders ──────────────────────────────────────────────────────────────────────────────────

Side = Literal["yes", "no"]
Action = Literal["buy", "sell"]


@dataclass(frozen=True, slots=True)
class OrderRequest:
    """One limit order as Susquatch builds it. Prices are pips; the expiry is mandatory.

    ``canonical_payload`` is the exact string ``docs/RUNTIME.md`` rule 4 feeds into the
    deterministic idempotency key, so two requests with the same payload always share a key.
    """

    exchange_id: str
    side: Side
    action: Action
    quantity: int
    price_pips: int
    expiration: datetime
    tournament_id: str

    def __post_init__(self) -> None:
        if self.side not in ("yes", "no"):
            raise ValueError(f"side must be yes/no, got {self.side!r}")
        if self.action not in ("buy", "sell"):
            raise ValueError(f"action must be buy/sell, got {self.action!r}")
        if not 0 < self.quantity <= 2_147_483_647:
            raise ValueError(f"quantity out of range: {self.quantity}")
        validate_limit(self.price_pips)
        if self.expiration.tzinfo is None:
            raise ValueError("expiration must be timezone-aware")
        if not self.tournament_id:
            raise ValueError("tournament_id is required")

    @property
    def expiration_unix_s(self) -> int:
        return int(self.expiration.timestamp())

    def canonical_payload(self) -> str:
        return (
            f"{self.exchange_id}|{self.side}|{self.action}|{self.quantity}|{self.price_pips}|"
            f"{self.expiration_unix_s}|{self.tournament_id}"
        )

    def to_body(self) -> dict[str, Any]:
        """JSON body fields shared by /orders, /orders/batch items and /orders/multi-leg legs."""
        return {
            "exchangeId": self.exchange_id,
            "side": self.side,
            "action": self.action,
            "quantity": self.quantity,
            "price": to_price(self.price_pips),
            "expirationDate": iso_utc(datetime.fromtimestamp(self.expiration_unix_s, UTC)),
            "tournamentId": self.tournament_id,
        }

    @property
    def yes_direction(self) -> int:
        """+1 if this order adds YES exposure (buy yes / sell no), -1 otherwise."""
        buys_yes = (self.side == "yes") == (self.action == "buy")
        return 1 if buys_yes else -1

    @property
    def yes_price_pips(self) -> int:
        """Limit expressed in YES space (NO prices are complemented)."""
        return self.price_pips if self.side == "yes" else 10_000 - self.price_pips


class AllEconomics(ApiModel):
    full_notional_cost: float | None = None
    effective_entry_cost: float | None = None
    collateral_savings: float | None = None
    relationship_ids: list[str] = Field(default_factory=list)
    component_id: str | None = None
    guaranteed_payout_floor_after: float | None = None
    outstanding_advance_after: float | None = None
    collateral_repayment: float | None = None
    net_buying_power_impact: float | None = None


class PlaceOrderResult(ApiModel):
    order_id: int | None = None
    exchange_id: str
    open: bool = False
    remaining_quantity: float | None = None
    action: str | None = None
    side: str | None = None
    price: float | None = None
    quantity: int | None = None
    terminal_reason_code: str | None = None
    quantity_traded: float = 0.0
    total_cost: float = 0.0
    fill_price: float | None = None
    all: AllEconomics | None = None


class BatchItemResult(ApiModel):
    index: int
    ok: bool
    status: int
    data: dict[str, Any] = Field(default_factory=dict)

    def order(self) -> PlaceOrderResult | None:
        return PlaceOrderResult.model_validate(self.data) if self.ok else None

    @property
    def error_code(self) -> str | None:
        if self.ok:
            return None
        err = self.data.get("error")
        if isinstance(err, dict):
            return str(err.get("code"))
        code = self.data.get("code")
        return str(code) if code is not None else None


class BatchResult(ApiModel):
    results: list[BatchItemResult]
    #: HTTP status of the envelope (200 / 207 / 422); set by the client, not the platform.
    http_status: int = 200


class MultiLegItem(ApiModel):
    index: int
    data: PlaceOrderResult


class MultiLegResult(ApiModel):
    results: list[MultiLegItem]


class Order(ApiModel):
    id: int
    exchange_id: str
    side: str
    action: str
    quantity: float
    price_limit: float | None = None
    open: bool
    created_at: datetime | None = None
    expiration_date: datetime | None = None


class OrdersPage(ApiModel):
    data: list[Order]
    pagination: Pagination = Field(default_factory=Pagination)


class CancelResult(ApiModel):
    order_id: int
    tournament_id: str | None = None
    message: str | None = None


class CancelAllError(ApiModel):
    order_id: int
    error: str


class CancelAllResult(ApiModel):
    cancelled: int
    errors: list[CancelAllError] = Field(default_factory=list)
    #: 200 all cancelled, 207 partial, 422 all failed (set by the client).
    http_status: int = 200

    @property
    def complete(self) -> bool:
        return self.http_status == 200 and not self.errors


# ── portfolio ───────────────────────────────────────────────────────────────────────────────


class PositionLot(ApiModel):
    lot_id: str
    side: str
    quantity: float
    entry_price: float
    opened_at: datetime | None = None


class Position(ApiModel):
    exchange_id: str
    market_id: str | None = None
    market_title: str | None = None
    option: str | None = None
    settled: bool = False
    quantity: float
    avg_cost: float | None = None
    current_price: float | None = None
    market_value: float | None = None
    cost_basis: float | None = None
    unrealized_pnl: float | None = None
    unrealized_pnl_pct: float | None = None
    money_earned: float | None = None
    lots: list[PositionLot] = Field(default_factory=list)


class PositionsSummary(ApiModel):
    total_market_value: float = 0.0
    total_cost_basis: float = 0.0
    total_unrealized_pnl: float = 0.0


class Positions(ApiModel):
    positions: list[Position]
    summary: PositionsSummary = Field(default_factory=PositionsSummary)


class PnL(ApiModel):
    period: str | None = None
    period_pnl: float | None = None
    unrealized_pnl: float | None = None
    total_account_value: float
    total_holdings_value: float | None = None
    total_cost_basis: float | None = None
    roi: float | None = None
    sharpe: float | None = None


class Fill(ApiModel):
    id: int
    order_id: int | None = None
    exchange_id: str
    market_id: str | None = None
    price: float | None = None
    quantity: float
    side: str
    filled_at: datetime | None = None


class FillsPage(ApiModel):
    data: list[Fill]
    pagination: Pagination = Field(default_factory=Pagination)


class OrderFill(ApiModel):
    """One fill of one order (``GET /orders/{id}/fills``): no exchange/order ids per item."""

    id: int
    price: float | None = None
    quantity: float
    side: str
    filled_at: datetime | None = None


class OrderFills(ApiModel):
    order_id: int
    exchange_id: str
    tournament_id: str | None = None
    data: list[OrderFill]
    pagination: Pagination = Field(default_factory=Pagination)
    total_quantity_filled: float = 0.0
    avg_fill_price: float | None = None


class Settlement(ApiModel):
    id: str
    type: str | None = None
    created_at: datetime | None = None
    market_id: str | None = None
    market_title: str | None = None
    exchange_id: str | None = None
    settlement_option: str | None = None
    settlement_outcome: str | None = None
    shares: float | None = None
    avg_entry_price: float | None = None
    cost_basis: float | None = None
    payout: float = 0.0
    realized_pnl: float | None = None


class SettlementsPage(ApiModel):
    data: list[Settlement]
    pagination: Pagination = Field(default_factory=Pagination)


class CollateralComponent(ApiModel):
    profile_id: str | None = None
    tournament_id: str | None = None
    outstanding_advance: float = 0.0
    component_id: str
    covered_exchange_ids: list[str] = Field(default_factory=list)
    relationship_ids: list[str] = Field(default_factory=list)
    guaranteed_payout_floor: float | None = None
    diagnostic: str | None = None


class Collateral(ApiModel):
    data: list[CollateralComponent]


# ── leaderboard ─────────────────────────────────────────────────────────────────────────────


class LeaderboardEntry(ApiModel):
    rank: int
    profile_id: str
    username: str | None = None
    pnl: float = 0.0
    trades_count: int = 0
    volume: float = 0.0
    win_rate: float | None = None
    roi: float = 0.0


class Leaderboard(ApiModel):
    leaderboard: list[LeaderboardEntry]
    total: int
    period: str | None = None
    limit: int | None = None
    offset: int | None = None
    my_rank: int | None = None


# ── relationships & constraints ─────────────────────────────────────────────────────────────


class RelationshipNode(ApiModel):
    exchange_id: str
    market_id: str | None = None
    market_title: str | None = None
    outcome: str | None = None
    current_price: float | None = None
    role: str | None = None
    contract_id: str | None = None


class RelationshipDirection(ApiModel):
    kind: str | None = None
    from_exchange_ids: list[str] = Field(default_factory=list)
    to_exchange_ids: list[str] = Field(default_factory=list)
    description: str | None = None


class ConstraintSemantics(ApiModel):
    description: str | None = None
    price_rule: str | None = None
    bound_semantics: str | None = None
    evaluation_endpoint: str | None = None


class Relationship(ApiModel):
    id: str
    type: str
    source: str | None = None
    status: str | None = None
    label: str | None = None
    reason: str | None = None
    is_exhaustive: bool = False
    expression: dict[str, Any] | None = None
    nodes: list[RelationshipNode] = Field(default_factory=list)
    direction: RelationshipDirection | None = None
    constraint: ConstraintSemantics | None = None
    version: int | None = None

    def exchange_ids(self, role: str | None = None) -> list[str]:
        return [n.exchange_id for n in self.nodes if role is None or n.role == role]


class RelationshipsPage(ApiModel):
    data: list[Relationship]
    pagination: Pagination = Field(default_factory=Pagination)


class ObservedPrice(ApiModel):
    exchange_id: str
    price: float
    market_id: str | None = None
    market_title: str | None = None
    outcome: str | None = None
    current_price: float | None = None


class CorrectiveTrade(ApiModel):
    exchange_id: str
    outcome_side: str
    action: str
    rationale: str | None = None
    market_id: str | None = None
    market_title: str | None = None
    outcome: str | None = None
    current_price: float | None = None


class ConstraintEvaluation(ApiModel):
    relationship_id: str
    type: str
    violation_amount: float = 0.0
    reason: str | None = None
    suggested_corrective_trades: list[CorrectiveTrade] = Field(default_factory=list)
    observed_prices: list[ObservedPrice] | None = None
    bound: float | None = None
    lower_bound: float | None = None
    upper_bound: float | None = None
    current_value: float | None = None
    evaluation_status: str
    diagnostic: str | None = None
    tournament_id: str | None = None
    direction: RelationshipDirection | None = None
    constraint: ConstraintSemantics | None = None
    computed_at: datetime | None = None


class ConstraintsResult(ApiModel):
    data: list[ConstraintEvaluation]
    violations_count: int = 0
    computed_at: datetime | None = None


# ── realtime ────────────────────────────────────────────────────────────────────────────────


class RealtimeToken(ApiModel):
    token: str = Field(repr=False)
    expires_at: datetime
    supabase_url: str
    anon_key: str
    channels: dict[str, str] = Field(default_factory=dict)
