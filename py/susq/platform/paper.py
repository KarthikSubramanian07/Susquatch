"""Paper broker: the live write interface, simulated against the latest REAL books.

``PaperBroker`` implements :class:`~susq.platform.client.TradingApi`, so the gateway can swap it in for
the live client without any strategy knowing. Semantics mirror the platform:

* limit orders only (market orders are never sent by Susquatch); on-tick validation happens in
  :class:`~susq.platform.models.OrderRequest`;
* sells canonicalize to complement buys unless fully backed by the paper position;
* the marketable part fills immediately **at book prices, walking depth**, against the book read
  through the client at that moment; liquidity we already took is remembered until the real book
  at that level changes, so two paper orders can't both eat the same displayed size;
* the remainder rests virtually and fills later only when a newer real book trades **strictly
  through** its limit (conservative: at an equal price we'd be queued behind real orders);
* self-trade prevention against our own paper orders, GTD expiry, idempotency replay/409, and
  insufficient-balance rejections behave like the platform;
* cash and FIFO positions use :class:`~susq.platform.ledger.Ledger`, the same code the mock uses;
* ALL collateral uses :class:`~susq.platform.collateral.CollateralAccount`, also shared with the mock:
  once :meth:`PaperBroker.set_relationships` is called, fills on linked exchanges get the modeled
  advance, responses carry ``all``, and a fill that would overdraw cash stops (remainder cancelled
  with ``InsufficientBalance``). ``collateral=False`` keeps plain cash accounting and ``all: null``.
"""

from __future__ import annotations

import copy
import itertools
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Protocol

import orjson

from susq.platform.collateral import AllTotals, CollateralAccount, CollateralGraph, Relation
from susq.platform.errors import ApiError
from susq.platform.ledger import Ledger, opening_cost_pips, units
from susq.platform.models import (
    AllEconomics,
    BatchItemResult,
    BatchResult,
    CancelAllResult,
    CancelResult,
    Collateral,
    CollateralComponent,
    MultiLegItem,
    MultiLegResult,
    Order,
    OrderBook,
    OrderRequest,
    PlaceOrderResult,
    Position,
    PositionLot,
    Positions,
    PositionsSummary,
)
from susq.platform.prices import PIPS_PER_UNIT, to_pips, to_price

__all__ = ["BookSource", "PaperBroker", "PaperOrder"]


class BookSource(Protocol):
    async def get_orderbook(self, exchange_id: str, tournament_id: str, *, depth: int = 20) -> OrderBook: ...


@dataclass(slots=True)
class PaperOrder:
    id: int
    exchange_id: str
    side: str  # canonical
    action: str
    price_pips: int  # side-relative
    yes_dir: int
    yes_limit: int
    quantity: int
    remaining: int
    created_at: datetime
    expiration: datetime
    status: str = "open"  # open | filled | cancelled | expired
    terminal_reason: str | None = None
    traded: int = 0
    cost_pips: int = 0
    commit_qty: int = 0  # remainder shares closing an existing paper position
    reserve_qty: int = 0  # remainder shares whose opening cost is reserved
    credit_pips: int = 0  # prospective ALL advance netted off the reservation
    totals: AllTotals = field(default_factory=AllTotals)

    def reserved_pips(self) -> int:
        return max(0, opening_cost_pips(self.yes_dir, self.yes_limit, self.reserve_qty) - self.credit_pips)


def _levels(book: OrderBook, yes_dir: int) -> list[tuple[int, int]]:
    """Opposite side of the book as (yes_price_pips, qty), best first."""
    levels = book.asks if yes_dir > 0 else book.bids
    out = [(to_pips(lv.price), round(lv.quantity)) for lv in levels]
    out.sort(key=lambda lv: lv[0] if yes_dir > 0 else -lv[0])
    return out


class PaperBroker:
    """Paper trading with the live write interface. Not thread-safe; one event loop."""

    def __init__(
        self,
        books: BookSource,
        tournament_id: str,
        *,
        starting_cash: float = 100_000.0,
        depth: int = 50,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
        relationships: Sequence[Relation] = (),
        collateral: bool = True,
    ) -> None:
        self._books = books
        self.tournament_id = tournament_id
        self.ledger = Ledger(round(starting_cash * PIPS_PER_UNIT))
        self.collateral = CollateralAccount(CollateralGraph(relationships), enabled=collateral)
        self._depth = depth
        self._clock = clock
        self.orders: dict[int, PaperOrder] = {}
        self._ids = itertools.count(900_000_001)
        self._idem: dict[tuple[str, str], tuple[bytes, object]] = {}
        #: (exchange, yes_price) -> (displayed qty when we took from it, qty we took)
        self._consumed: dict[tuple[str, int], tuple[int, int]] = {}
        self.marks: dict[str, int] = {}

    # ── helpers ─────────────────────────────────────────────────────────────────────────────

    def set_relationships(self, relationships: Sequence[Relation]) -> None:
        """Refresh the relationship graph (advances already granted are kept, keyed by component)."""
        self.collateral.graph = CollateralGraph(relationships)

    def _reserved_pips(self) -> int:
        return sum(o.reserved_pips() for o in self.orders.values() if o.status == "open")

    def _committed(self, exchange_id: str) -> int:
        return sum(
            o.commit_qty for o in self.orders.values() if o.status == "open" and o.exchange_id == exchange_id
        )

    @property
    def available_cash(self) -> float:
        return units(self.ledger.cash_pips - self._reserved_pips())

    def _expire(self, now: datetime) -> None:
        for o in self.orders.values():
            if o.status == "open" and o.expiration <= now:
                self._close(o, "expired")

    @staticmethod
    def _close(order: PaperOrder, status: str, reason: str | None = None) -> None:
        order.status = status
        order.terminal_reason = reason
        order.commit_qty = order.reserve_qty = 0

    def _available(self, exchange_id: str, price: int, displayed: int) -> int:
        seen = self._consumed.get((exchange_id, price))
        if seen is None or seen[0] != displayed:  # the real level changed: our take is stale
            self._consumed.pop((exchange_id, price), None)
            return displayed
        return max(0, displayed - seen[1])

    def _take(self, exchange_id: str, price: int, displayed: int, n: int) -> None:
        seen = self._consumed.get((exchange_id, price))
        taken = seen[1] if seen is not None and seen[0] == displayed else 0
        self._consumed[(exchange_id, price)] = (displayed, taken + n)

    def _idempotent(self, endpoint: str, key: str, payload: object) -> object | None:
        fingerprint = orjson.dumps(payload, option=orjson.OPT_SORT_KEYS)
        stored = self._idem.get((endpoint, key))
        if stored is None:
            return None
        if stored[0] != fingerprint:
            raise ApiError(409, "CONFLICT", "Idempotency key reused with a different payload.")
        return stored[1]

    def _remember(self, endpoint: str, key: str, payload: object, result: object) -> None:
        self._idem[(endpoint, key)] = (orjson.dumps(payload, option=orjson.OPT_SORT_KEYS), result)

    # ── placement ───────────────────────────────────────────────────────────────────────────

    async def _place(self, req: OrderRequest) -> PlaceOrderResult:
        if req.tournament_id != self.tournament_id:
            raise ApiError(404, "NOT_FOUND", "Tournament not found.")
        now = self._clock()
        if req.expiration <= now:
            raise ApiError(400, "VALIDATION_ERROR", "expirationDate must be in the future.")
        self._expire(now)
        ex = req.exchange_id
        side, action, price = req.side, req.action, req.price_pips
        if action == "sell":
            pos = self.ledger.position(ex)
            held = pos if side == "yes" else -pos
            if held - self._committed(ex) < req.quantity:
                side = "no" if side == "yes" else "yes"
                action, price = "buy", PIPS_PER_UNIT - price
        yes_dir = 1 if (side == "yes") == (action == "buy") else -1
        yes_limit = price if side == "yes" else PIPS_PER_UNIT - price
        if action == "sell":
            commit, reserve = req.quantity, 0
        else:
            commit = min(req.quantity, max(0, self.ledger.closable(ex, yes_dir) - self._committed(ex)))
            reserve = req.quantity - commit
        need, credit = self.collateral.required_cash_pips(
            self.ledger, ex, yes_dir, yes_limit, req.quantity, reserve
        )
        if need > self.ledger.cash_pips - self._reserved_pips():
            raise ApiError(
                400,
                "VALIDATION_ERROR",
                "Insufficient balance.",
                {"reason": "insufficient_balance", "required": units(need)},
            )
        order = PaperOrder(
            id=next(self._ids),
            exchange_id=ex,
            side=side,
            action=action,
            price_pips=price,
            yes_dir=yes_dir,
            yes_limit=yes_limit,
            quantity=req.quantity,
            remaining=req.quantity,
            created_at=now,
            expiration=req.expiration,
            commit_qty=commit,
            reserve_qty=reserve,
            credit_pips=credit,
        )
        book = await self._books.get_orderbook(ex, self.tournament_id, depth=self._depth)
        self.orders[order.id] = order
        own_cross = any(
            o.status == "open"
            and o.id != order.id
            and o.exchange_id == ex
            and o.yes_dir == -yes_dir
            and (o.yes_limit <= yes_limit if yes_dir > 0 else o.yes_limit >= yes_limit)
            for o in self.orders.values()
        )
        starved = self._match(order, book, now, strictly_through=False)
        if order.remaining == 0:
            self._close(order, "filled")
        elif own_cross:
            # Our own resting paper order would cross: other counterparties filled, remainder cancelled.
            self._close(order, "cancelled", "SelfTradePrevented")
        elif starved:
            self._close(order, "cancelled", "InsufficientBalance")
        return self._result(order)

    def _match(self, order: PaperOrder, book: OrderBook, now: datetime, *, strictly_through: bool) -> bool:
        """Fill ``order`` against ``book``. True if it stopped because cash ran out (ALL repayment)."""
        for price, displayed in _levels(book, order.yes_dir):
            if order.remaining == 0:
                break
            if order.yes_dir > 0:
                crosses = price < order.yes_limit if strictly_through else price <= order.yes_limit
            else:
                crosses = price > order.yes_limit if strictly_through else price >= order.yes_limit
            if not crosses:
                break
            n = min(order.remaining, self._available(order.exchange_id, price, displayed))
            if n <= 0:
                continue
            # Resting orders that fill later fill at their own limit; takers fill at the book price.
            fill_price = order.yes_limit if strictly_through else price
            n = self.collateral.affordable(self.ledger, order.exchange_id, order.yes_dir, fill_price, n)
            if n <= 0:
                return True
            self._take(order.exchange_id, price, displayed, n)
            econ = self.collateral.execute(self.ledger, order.exchange_id, order.yes_dir * n, fill_price, now)
            order.totals.add(econ)
            order.credit_pips = max(0, order.credit_pips - econ.savings_pips)
            from_commit = min(n, order.commit_qty)
            order.commit_qty -= from_commit
            order.reserve_qty = max(0, order.reserve_qty - (n - from_commit))
            order.remaining -= n
            order.traded += n
            side_price = fill_price if order.side == "yes" else PIPS_PER_UNIT - fill_price
            order.cost_pips += n * side_price
            self.marks[order.exchange_id] = fill_price
        return False

    def _result(self, order: PaperOrder) -> PlaceOrderResult:
        wire = order.totals.wire(self.collateral, self.ledger, order.exchange_id)
        return PlaceOrderResult(
            order_id=order.id,
            exchange_id=order.exchange_id,
            open=order.status == "open",
            remaining_quantity=order.remaining if order.status == "open" else 0,
            action=order.action,
            side=order.side,
            price=to_price(order.price_pips),
            quantity=order.quantity,
            terminal_reason_code=order.terminal_reason,
            quantity_traded=order.traded,
            total_cost=units(order.cost_pips),
            fill_price=units(order.cost_pips) / order.traded if order.traded else None,
            all=AllEconomics.model_validate(wire) if wire is not None else None,
        )

    # ── TradingApi ──────────────────────────────────────────────────────────────────────────

    async def place_order(self, order: OrderRequest, idempotency_key: str) -> PlaceOrderResult:
        payload = order.to_body()
        stored = self._idempotent("orders", idempotency_key, payload)
        if stored is not None:
            assert isinstance(stored, PlaceOrderResult)
            return stored
        result = await self._place(order)
        self._remember("orders", idempotency_key, payload, result)
        return result

    async def place_batch(self, orders: Sequence[OrderRequest], idempotency_key: str) -> BatchResult:
        payload = [o.to_body() for o in orders]
        stored = self._idempotent("batch", idempotency_key, payload)
        if stored is not None:
            assert isinstance(stored, BatchResult)
            return stored
        items: list[BatchItemResult] = []
        for i, order in enumerate(orders):
            try:
                res = await self._place(order)
            except ApiError as err:
                items.append(
                    BatchItemResult(
                        index=i,
                        ok=False,
                        status=err.status,
                        data={"error": {"code": err.code, "message": err.message}},
                    )
                )
            else:
                items.append(
                    BatchItemResult(index=i, ok=True, status=200, data=res.model_dump(by_alias=True))
                )
        oks = sum(1 for it in items if it.ok)
        result = BatchResult(results=items, http_status=200 if oks == len(items) else (207 if oks else 422))
        self._remember("batch", idempotency_key, payload, result)
        return result

    async def place_multi_leg(
        self,
        legs: Sequence[OrderRequest],
        idempotency_key: str,
        relationship_constraint: str | None = None,
    ) -> MultiLegResult:
        if relationship_constraint is not None:
            raise ApiError(422, "RELATIONSHIP_VIOLATION", "Paper broker does not evaluate relationships.")
        if len({leg.exchange_id for leg in legs}) != len(legs):
            raise ApiError(400, "VALIDATION_ERROR", "Duplicate legs for the same exchange.")
        payload = [leg.to_body() for leg in legs]
        stored = self._idempotent("multi-leg", idempotency_key, payload)
        if stored is not None:
            assert isinstance(stored, MultiLegResult)
            return stored
        snapshot = (
            copy.deepcopy(self.ledger),
            copy.deepcopy(self.orders),
            dict(self._consumed),
            copy.deepcopy(self.collateral),
        )
        items: list[MultiLegItem] = []
        for i, leg in enumerate(legs):
            try:
                items.append(MultiLegItem(index=i, data=await self._place(leg)))
            except ApiError as err:
                self.ledger, self.orders, self._consumed, self.collateral = snapshot
                raise ApiError(err.status, err.code, err.message, {**err.details, "leg": i}) from err
        result = MultiLegResult(results=items)
        self._remember("multi-leg", idempotency_key, payload, result)
        return result

    async def cancel_order(self, order_id: int) -> CancelResult:
        self._expire(self._clock())
        order = self.orders.get(order_id)
        if order is None:
            raise ApiError(404, "NOT_FOUND", "Order not found.")
        if order.status != "open":
            raise ApiError(409, "CONFLICT", "Order is already closed.")
        self._close(order, "cancelled")
        return CancelResult(order_id=order_id, tournament_id=self.tournament_id, message="Order cancelled.")

    async def cancel_all(
        self, tournament_id: str, *, exchange_id: str | None = None, market_id: str | None = None
    ) -> CancelAllResult:
        if tournament_id != self.tournament_id:
            raise ApiError(403, "FORBIDDEN", "Not a member of this tournament.")
        if market_id is not None:
            raise ApiError(400, "VALIDATION_ERROR", "Paper broker cancels by exchange or all.")
        self._expire(self._clock())
        n = 0
        for o in self.orders.values():
            if o.status == "open" and (exchange_id is None or o.exchange_id == exchange_id):
                self._close(o, "cancelled")
                n += 1
        return CancelAllResult(cancelled=n, errors=[], http_status=200)

    async def list_open_orders(
        self, tournament_id: str, *, exchange_id: str | None = None, market_id: str | None = None
    ) -> list[Order]:
        self._expire(self._clock())
        return [
            Order(
                id=o.id,
                exchange_id=o.exchange_id,
                side=o.side,
                action=o.action,
                quantity=o.remaining,
                price_limit=to_price(o.price_pips),
                open=True,
                created_at=o.created_at,
                expiration_date=o.expiration,
            )
            for o in sorted(self.orders.values(), key=lambda o: o.id)
            if o.status == "open" and (exchange_id is None or o.exchange_id == exchange_id)
        ]

    # ── resting-order simulation ────────────────────────────────────────────────────────────

    def on_book(self, book: OrderBook) -> int:
        """Fill resting paper orders that ``book`` trades strictly through. Returns shares filled."""
        now = self._clock()
        self._expire(now)
        filled = 0
        for o in sorted(self.orders.values(), key=lambda o: o.id):
            if o.status != "open" or o.exchange_id != book.exchange_id:
                continue
            before = o.remaining
            starved = self._match(o, book, now, strictly_through=True)
            filled += before - o.remaining
            if o.remaining == 0:
                self._close(o, "filled")
            elif starved:
                self._close(o, "cancelled", "InsufficientBalance")
        return filled

    async def poll(self) -> int:
        """Refresh books for every exchange with resting paper orders and apply :meth:`on_book`."""
        self._expire(self._clock())
        exchanges = sorted({o.exchange_id for o in self.orders.values() if o.status == "open"})
        filled = 0
        for ex in exchanges:
            filled += self.on_book(await self._books.get_orderbook(ex, self.tournament_id, depth=self._depth))
        return filled

    # ── portfolio ───────────────────────────────────────────────────────────────────────────

    def positions(self) -> Positions:
        rows: list[Position] = []
        total_value = total_basis = 0.0
        for ex, h in sorted(self.ledger.holdings.items()):
            if h.quantity == 0:
                continue
            mark = self.marks.get(ex)
            basis = units(h.cost_basis_pips)
            value = units(h.market_value_pips(mark)) if mark is not None else basis
            rows.append(
                Position(
                    exchange_id=ex,
                    quantity=h.quantity,
                    avg_cost=basis / abs(h.quantity),
                    current_price=to_price(mark) if mark is not None else None,
                    market_value=value,
                    cost_basis=basis,
                    unrealized_pnl=value - basis,
                    money_earned=units(h.realized_pips),
                    lots=[
                        PositionLot(
                            lot_id=lot.lot_id,
                            side=lot.side,
                            quantity=lot.quantity,
                            entry_price=to_price(lot.entry_pips),
                            opened_at=lot.opened_at,
                        )
                        for lot in h.lots
                    ],
                )
            )
            total_value += value
            total_basis += basis
        return Positions(
            positions=rows,
            summary=PositionsSummary(
                total_market_value=total_value,
                total_cost_basis=total_basis,
                total_unrealized_pnl=total_value - total_basis,
            ),
        )

    def account_value(self) -> float:
        """Cash + marked holdings - outstanding ALL advance (collateral model assumption A4)."""
        return units(self.ledger.account_value_pips(self.marks) - self.collateral.liability_pips)

    def get_collateral(self) -> Collateral:
        """The paper ``GET /portfolio/collateral``."""
        return Collateral(
            data=[
                CollateralComponent.model_validate({"tournamentId": self.tournament_id, **row})
                for row in self.collateral.views(self.ledger)
            ]
        )
