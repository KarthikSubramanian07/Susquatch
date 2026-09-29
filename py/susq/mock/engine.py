"""In-memory matching engine that follows The Super Market's documented order semantics.

What it implements (each point is from the official reference, see ``py/susq/mock/README.md``):

* One YES-space book per exchange, price-time priority, trades at the resting order's price.
* Limit prices on the 0.005 tick within [0.005, 0.995] (else ``400 VALIDATION_ERROR``). Market
  orders: ``price`` omitted, ``price: 1`` on a buy or ``price: 0`` on a sell; the unfilled remainder
  of a market order is cancelled. ``expirationDate`` is rejected on market orders.
* Sell canonicalization: a fully-backed SELL closes conventionally; a SELL that is flat, opposite the
  held side, or larger than the held quantity becomes the complement BUY
  (``sell yes q@p`` == ``buy no q@(1-p)``). Responses report the canonical action/side/price/quantity.
* Buying the side opposite a held position nets FIFO and pays 1 per cancelled pair (see
  :mod:`susq.platform.ledger`).
* Self-trade prevention: an order fills other counterparties, then any remainder that would cross
  the caller's own resting order is cancelled with ``terminalReasonCode: "SelfTradePrevented"``.
* GTD: orders past ``expirationDate`` are removed lazily at read/match time. Expiry emits nothing.
* Balance checks: a limit order reserves the cash its opening part could consume; if the available
  cash is short the order is rejected with ``400`` (``details.reason = "insufficient_balance"``).
"""

from __future__ import annotations

import copy
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

from susq.platform.ledger import Ledger, opening_cost_pips, units
from susq.platform.prices import (
    MAX_PIPS,
    MIN_PIPS,
    PIPS_PER_UNIT,
    TICK_PIPS,
    to_price,
)

__all__ = [
    "Account",
    "Engine",
    "EngineError",
    "EngineState",
    "FillRecord",
    "MarketDef",
    "RestingOrder",
    "TradeRecord",
]

MAX_QTY = 2_147_483_647


class EngineError(Exception):
    """A request the platform would reject; rendered as the standard error envelope."""

    def __init__(
        self,
        status: int,
        code: str,
        message: str,
        details: dict[str, Any] | None = None,
        *,
        headers: dict[str, str] | None = None,
        extra: dict[str, Any] | None = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        self.details = details or {}
        self.headers = headers or {}
        self.extra = extra or {}

    def envelope(self) -> dict[str, Any]:
        err: dict[str, Any] = {"code": self.code, "message": self.message}
        if self.details:
            err["details"] = self.details
        return {"error": err, **self.extra}


def iso(ts: datetime) -> str:
    return ts.astimezone(UTC).isoformat().replace("+00:00", "Z")


def parse_iso(value: str) -> datetime:
    ts = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if ts.tzinfo is None:
        raise ValueError("timestamp without timezone")
    return ts


@dataclass(slots=True)
class MarketDef:
    market_id: str
    exchange_id: str
    title: str
    category: str = "election-outcome"
    status: str = "open"
    option: str = "YES"
    initial_price: float | None = None
    settlement_date: str = "2026-11-30T00:00:00Z"
    contract_details: dict[str, Any] = field(default_factory=dict)


@dataclass(slots=True)
class Account:
    profile_id: str
    username: str
    ledger: Ledger
    api_key: str | None = None
    is_bot: bool = False
    trades_count: int = 0
    volume_pips: int = 0
    initial_pips: int = 100_000 * PIPS_PER_UNIT


@dataclass(slots=True)
class RestingOrder:
    id: int
    owner: str
    exchange_id: str
    side: str  # canonical, as placed
    action: str
    price_pips: int  # side-relative limit
    yes_dir: int  # +1 bid (YES-buying), -1 ask (YES-selling)
    yes_price: int
    quantity: int
    remaining: int
    seq: int
    created_at: datetime
    expiration: datetime | None
    status: str = "open"  # open | filled | cancelled | expired
    reserve_qty: int = 0  # shares of the remainder whose opening cost is reserved
    commit_qty: int = 0  # shares of the remainder that close an existing position
    terminal_reason: str | None = None

    def reserved_pips(self) -> int:
        return opening_cost_pips(self.yes_dir, self.yes_price, self.reserve_qty)


@dataclass(slots=True)
class TradeRecord:
    id: int
    exchange_id: str
    market_id: str
    yes_price: int
    size: int
    taker_side: str  # "YES" | "NO": which space the taker traded in
    created_at: datetime
    buyer: str  # YES-buying account
    seller: str


@dataclass(slots=True)
class FillRecord:
    id: int
    order_id: int
    owner: str
    exchange_id: str
    market_id: str
    yes_price: int
    quantity: int  # outcome-signed: negative on the NO side
    side: str
    filled_at: datetime


@dataclass
class EngineState:
    """Everything mutable, so atomic multi-leg orders can snapshot and roll back."""

    accounts: dict[str, Account] = field(default_factory=dict)
    orders: dict[int, RestingOrder] = field(default_factory=dict)
    bids: dict[str, list[RestingOrder]] = field(default_factory=dict)
    asks: dict[str, list[RestingOrder]] = field(default_factory=dict)
    trades: list[TradeRecord] = field(default_factory=list)
    fills: list[FillRecord] = field(default_factory=list)
    latest: dict[str, int] = field(default_factory=dict)
    next_order_id: int = 1001
    next_seq: int = 1
    next_trade_id: int = 1
    next_fill_id: int = 1


@dataclass(frozen=True, slots=True)
class Placement:
    """Result of one placement plus side effects the server broadcasts."""

    body: dict[str, Any]
    trades: list[TradeRecord]
    touched: set[str]


class Engine:
    """The exchange for one tournament."""

    def __init__(self, tournament_id: str, markets: list[MarketDef]) -> None:
        self.tournament_id = tournament_id
        self.markets: dict[str, MarketDef] = {m.market_id: m for m in markets}
        self.by_exchange: dict[str, MarketDef] = {m.exchange_id: m for m in markets}
        self.state = EngineState()
        for m in markets:
            self.state.bids[m.exchange_id] = []
            self.state.asks[m.exchange_id] = []

    # ── accounts ────────────────────────────────────────────────────────────────────────────

    def add_account(self, account: Account) -> Account:
        self.state.accounts[account.profile_id] = account
        return account

    def account(self, profile_id: str) -> Account:
        return self.state.accounts[profile_id]

    def reserved_pips(self, profile_id: str) -> int:
        orders = self.state.orders.values()
        return sum(o.reserved_pips() for o in orders if o.owner == profile_id and o.status == "open")

    def committed(self, profile_id: str, exchange_id: str) -> int:
        return sum(
            o.commit_qty
            for o in self.state.orders.values()
            if o.owner == profile_id and o.exchange_id == exchange_id and o.status == "open"
        )

    def available_pips(self, profile_id: str) -> int:
        return self.account(profile_id).ledger.cash_pips - self.reserved_pips(profile_id)

    # ── snapshots (atomic multi-leg) ────────────────────────────────────────────────────────

    def snapshot(self) -> EngineState:
        return copy.deepcopy(self.state)

    def restore(self, state: EngineState) -> None:
        self.state = state

    # ── expiry ──────────────────────────────────────────────────────────────────────────────

    def sweep_expired(self, now: datetime) -> set[str]:
        """Remove orders past their expiration. Returns touched exchange ids."""
        touched: set[str] = set()
        for order in self.state.orders.values():
            if order.status == "open" and order.expiration is not None and order.expiration <= now:
                self._close(order, "expired")
                touched.add(order.exchange_id)
        return touched

    def _close(self, order: RestingOrder, status: str, reason: str | None = None) -> None:
        order.status = status
        order.terminal_reason = reason
        order.reserve_qty = 0
        order.commit_qty = 0
        book = self.state.bids if order.yes_dir > 0 else self.state.asks
        levels = book[order.exchange_id]
        if order in levels:
            levels.remove(order)

    # ── validation ──────────────────────────────────────────────────────────────────────────

    def validate(self, body: Any, now: datetime) -> dict[str, Any]:
        """Validate one order body without touching state. Returns a normalized dict."""
        if not isinstance(body, dict):
            raise EngineError(400, "VALIDATION_ERROR", "Order must be an object.")
        errors: dict[str, list[str]] = {}
        exchange_id = body.get("exchangeId")
        side = body.get("side")
        action = body.get("action")
        quantity = body.get("quantity")
        price = body.get("price")
        expiration_raw = body.get("expirationDate")
        tournament_id = body.get("tournamentId")

        if not isinstance(exchange_id, str) or not exchange_id.isdigit():
            errors["exchangeId"] = ["Expected a numeric string."]
        if side not in ("yes", "no"):
            errors["side"] = ["Expected 'yes' or 'no'."]
        if action not in ("buy", "sell"):
            errors["action"] = ["Expected 'buy' or 'sell'."]
        if isinstance(quantity, bool) or not isinstance(quantity, int) or not 0 < quantity <= MAX_QTY:
            errors["quantity"] = [f"Expected an integer in [1, {MAX_QTY}]."]
        is_market = price is None or (action == "buy" and price == 1) or (action == "sell" and price == 0)
        price_pips: int | None = None
        if price is not None:
            if isinstance(price, bool) or not isinstance(price, int | float) or not 0 <= price <= 1:
                errors["price"] = ["Expected a number in [0, 1]."]
            elif not is_market:
                raw = price * PIPS_PER_UNIT
                price_pips = round(raw)
                if abs(raw - price_pips) > 1e-6 or price_pips % TICK_PIPS:
                    errors["price"] = ["Limit prices must be on the 0.005 tick."]
                elif not MIN_PIPS <= price_pips <= MAX_PIPS:
                    errors["price"] = ["Limit prices must be between 0.005 and 0.995."]
        expiration: datetime | None = None
        if expiration_raw is not None:
            if is_market:
                errors["expirationDate"] = ["Not supported for market orders."]
            else:
                try:
                    expiration = parse_iso(str(expiration_raw))
                except ValueError:
                    errors["expirationDate"] = ["Expected an ISO 8601 datetime."]
                else:
                    if expiration <= now:
                        errors["expirationDate"] = ["Must be in the future."]
        if tournament_id is None:
            # Deliberately stricter than production: there, an omitted tournamentId silently routes to
            # the organization's default tournament. The mock turns that footgun into a hard error.
            errors["tournamentId"] = ["The mock requires an explicit tournamentId."]
        if errors:
            raise EngineError(400, "VALIDATION_ERROR", "Invalid request body.", errors)
        if tournament_id != self.tournament_id:
            raise EngineError(404, "NOT_FOUND", "Tournament not found.")
        if exchange_id not in self.by_exchange:
            raise EngineError(404, "NOT_FOUND", "Exchange not found.")
        return {
            "exchange_id": exchange_id,
            "side": side,
            "action": action,
            "quantity": quantity,
            "price_pips": price_pips,
            "is_market": is_market,
            "expiration": expiration,
        }

    # ── placement ───────────────────────────────────────────────────────────────────────────

    def place(self, owner: str, body: Any, now: datetime) -> Placement:
        """Validate, canonicalize, match and (maybe) rest one order."""
        order = self.validate(body, now)
        market = self.by_exchange[order["exchange_id"]]
        if market.status != "open":
            raise EngineError(403, "FORBIDDEN", "Market is not open for trading.")
        touched = self.sweep_expired(now)
        return self._execute(owner, order, now, touched)

    def _canonicalize(self, owner: str, order: dict[str, Any]) -> tuple[str, str, int | None, int]:
        """Return canonical (side, action, side-relative price, yes_dir)."""
        side, action, price = order["side"], order["action"], order["price_pips"]
        ex, qty = order["exchange_id"], order["quantity"]
        if action == "sell":
            pos = self.account(owner).ledger.position(ex)
            held = pos if side == "yes" else -pos
            available = held - self.committed(owner, ex)
            if available < qty:  # flat, opposite, or larger than held -> complement buy
                side = "no" if side == "yes" else "yes"
                action = "buy"
                price = None if price is None else PIPS_PER_UNIT - price
        buys_yes = (side == "yes") == (action == "buy")
        return side, action, price, 1 if buys_yes else -1

    def _execute(self, owner: str, order: dict[str, Any], now: datetime, touched: set[str]) -> Placement:
        acct = self.account(owner)
        ex = order["exchange_id"]
        qty = order["quantity"]
        side, action, price, yes_dir = self._canonicalize(owner, order)
        is_market = order["is_market"]
        # Limit in YES space; market orders cross anything.
        if is_market:
            yes_limit = PIPS_PER_UNIT if yes_dir > 0 else 0
        else:
            assert price is not None
            yes_limit = price if side == "yes" else PIPS_PER_UNIT - price

        if action == "sell":
            commit_qty, reserve_qty = qty, 0  # a backed sell only closes
        else:
            free_to_close = max(0, acct.ledger.closable(ex, yes_dir) - self.committed(owner, ex))
            commit_qty = min(qty, free_to_close)
            reserve_qty = 0 if is_market else qty - commit_qty
        if not is_market:
            need = opening_cost_pips(yes_dir, yes_limit, reserve_qty)
            if need > self.available_pips(owner):
                raise EngineError(
                    400,
                    "VALIDATION_ERROR",
                    "Insufficient balance.",
                    {"reason": "insufficient_balance", "required": units(need)},
                )

        st = self.state
        resting = RestingOrder(
            id=st.next_order_id,
            owner=owner,
            exchange_id=ex,
            side=side,
            action=action,
            price_pips=price if price is not None else 0,
            yes_dir=yes_dir,
            yes_price=yes_limit,
            quantity=qty,
            remaining=qty,
            seq=st.next_seq,
            created_at=now,
            expiration=order["expiration"],
            reserve_qty=reserve_qty,
            commit_qty=commit_qty,
        )
        st.next_order_id += 1
        st.next_seq += 1
        st.orders[resting.id] = resting

        opposite = st.asks[ex] if yes_dir > 0 else st.bids[ex]
        trades: list[TradeRecord] = []
        traded = 0
        cost_pips = 0
        self_cross = False
        for maker in list(opposite):
            if resting.remaining == 0:
                break
            crosses = maker.yes_price <= yes_limit if yes_dir > 0 else maker.yes_price >= yes_limit
            if not crosses:
                break
            if maker.owner == owner:
                self_cross = True
                continue
            n = min(resting.remaining, maker.remaining)
            if is_market:
                # Market orders reserve nothing up front: fill only what the cash covers.
                unit = opening_cost_pips(yes_dir, maker.yes_price, 1)
                open_part = max(0, n - acct.ledger.closable(ex, yes_dir))
                affordable = max(0, self.available_pips(owner)) // unit if unit else open_part
                if open_part > affordable:
                    n -= open_part - affordable
                    if n <= 0:
                        break
            trade = self._fill(resting, maker, n, now)
            trades.append(trade)
            traded += n
            side_price = maker.yes_price if side == "yes" else PIPS_PER_UNIT - maker.yes_price
            cost_pips += n * side_price

        if traded:
            touched.add(ex)
        if resting.remaining == 0:
            resting.status = "filled"
            resting.reserve_qty = resting.commit_qty = 0
        elif self_cross:
            self._close(resting, "cancelled", "SelfTradePrevented")
        elif is_market:
            self._close(resting, "cancelled")
        else:
            book = st.bids[ex] if yes_dir > 0 else st.asks[ex]
            book.append(resting)
            if yes_dir > 0:
                book.sort(key=lambda o: (-o.yes_price, o.seq))
            else:
                book.sort(key=lambda o: (o.yes_price, o.seq))
            touched.add(ex)

        body = {
            "orderId": resting.id,
            "exchangeId": ex,
            "open": resting.status == "open",
            "remainingQuantity": resting.remaining if resting.status == "open" else 0,
            "action": action,
            "side": side,
            "price": to_price(price) if price is not None else (1.0 if action == "buy" else 0.0),
            "quantity": qty,
            "terminalReasonCode": resting.terminal_reason,
            "quantityTraded": traded,
            "totalCost": units(cost_pips),
            "fillPrice": units(cost_pips) / traded if traded else None,
            "all": None,
        }
        return Placement(body=body, trades=trades, touched=touched)

    def _fill(self, taker: RestingOrder, maker: RestingOrder, n: int, now: datetime) -> TradeRecord:
        st = self.state
        price = maker.yes_price
        for order in (taker, maker):
            acct = self.account(order.owner)
            acct.ledger.execute(order.exchange_id, order.yes_dir * n, price, now)
            acct.trades_count += 1
            acct.volume_pips += n * price
            # Closing shares are consumed first, then the reserved opening shares.
            from_commit = min(n, order.commit_qty)
            order.commit_qty -= from_commit
            order.reserve_qty = max(0, order.reserve_qty - (n - from_commit))
            order.remaining -= n
            market = self.by_exchange[order.exchange_id]
            st.fills.append(
                FillRecord(
                    id=st.next_fill_id,
                    order_id=order.id,
                    owner=order.owner,
                    exchange_id=order.exchange_id,
                    market_id=market.market_id,
                    yes_price=price,
                    quantity=n if order.side == "yes" else -n,
                    side=order.side,
                    filled_at=now,
                )
            )
            st.next_fill_id += 1
        if maker.remaining == 0:
            self._close(maker, "filled")
        buyer, seller = (taker.owner, maker.owner) if taker.yes_dir > 0 else (maker.owner, taker.owner)
        trade = TradeRecord(
            id=st.next_trade_id,
            exchange_id=taker.exchange_id,
            market_id=self.by_exchange[taker.exchange_id].market_id,
            yes_price=price,
            size=n,
            taker_side=taker.side.upper(),
            created_at=now,
            buyer=buyer,
            seller=seller,
        )
        st.next_trade_id += 1
        st.trades.append(trade)
        st.latest[taker.exchange_id] = price
        return trade

    def print_trade(
        self, buyer: str, seller: str, exchange_id: str, yes_price: int, size: int, now: datetime
    ) -> TradeRecord:
        """Record an off-book trade between two accounts (scenario seeding / admin only)."""
        st = self.state
        for owner, yes_dir in ((buyer, 1), (seller, -1)):
            acct = self.account(owner)
            acct.ledger.execute(exchange_id, yes_dir * size, yes_price, now)
            acct.trades_count += 1
            acct.volume_pips += size * yes_price
        trade = TradeRecord(
            id=st.next_trade_id,
            exchange_id=exchange_id,
            market_id=self.by_exchange[exchange_id].market_id,
            yes_price=yes_price,
            size=size,
            taker_side="YES",
            created_at=now,
            buyer=buyer,
            seller=seller,
        )
        st.next_trade_id += 1
        st.trades.append(trade)
        st.latest[exchange_id] = yes_price
        return trade

    # ── cancels ─────────────────────────────────────────────────────────────────────────────

    def cancel(self, owner: str, order_id: int, now: datetime) -> RestingOrder:
        self.sweep_expired(now)
        order = self.state.orders.get(order_id)
        if order is None:
            raise EngineError(404, "NOT_FOUND", "Order not found.")
        if order.owner != owner:
            raise EngineError(403, "FORBIDDEN", "Order belongs to another user.")
        if order.status != "open":
            raise EngineError(409, "CONFLICT", "Order is already closed.")
        self._close(order, "cancelled")
        return order

    def open_orders(
        self, owner: str, now: datetime, *, exchange_id: str | None = None, market_id: str | None = None
    ) -> list[RestingOrder]:
        self.sweep_expired(now)
        out = []
        for order in self.state.orders.values():
            if order.owner != owner or order.status != "open":
                continue
            if exchange_id is not None and order.exchange_id != exchange_id:
                continue
            if market_id is not None and self.by_exchange[order.exchange_id].market_id != market_id:
                continue
            out.append(order)
        return sorted(out, key=lambda o: o.id)

    # ── reads ───────────────────────────────────────────────────────────────────────────────

    def book(self, exchange_id: str, now: datetime, depth: int = 20) -> dict[str, list[tuple[int, int]]]:
        """Aggregated YES book: bids descending, asks ascending, as (price_pips, qty)."""
        self.sweep_expired(now)

        def agg(orders: list[RestingOrder]) -> list[tuple[int, int]]:
            levels: dict[int, int] = {}
            for o in orders:
                levels[o.yes_price] = levels.get(o.yes_price, 0) + o.remaining
            return list(levels.items())

        bids = sorted(agg(self.state.bids[exchange_id]), key=lambda lv: -lv[0])[:depth]
        asks = sorted(agg(self.state.asks[exchange_id]), key=lambda lv: lv[0])[:depth]
        return {"bids": bids, "asks": asks}

    def best(self, exchange_id: str, now: datetime) -> tuple[int | None, int | None]:
        b = self.book(exchange_id, now, depth=1)
        return (b["bids"][0][0] if b["bids"] else None, b["asks"][0][0] if b["asks"] else None)

    def mark(self, exchange_id: str) -> int | None:
        """Tournament valuation price: the last tournament trade (null until one happens)."""
        return self.state.latest.get(exchange_id)

    def account_value_pips(self, profile_id: str) -> int:
        ledger = self.account(profile_id).ledger
        marks = dict(self.state.latest)
        return ledger.account_value_pips(marks)
