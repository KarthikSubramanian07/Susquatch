"""Order management: what we have resting, reconciled from the platform, and stale-input cancels.

The OMS keeps a *hint* of our open orders (updated from placement responses and cancels) so risk can
count resting exposure and refuse self-crossing orders without a round trip. It never trusts that
hint for long: every ``reconcile_interval_s``, and immediately on every realtime resync signal, it
replaces its view with ``GET /orders?status=open`` + tournament positions + account value.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import TYPE_CHECKING

from susq.platform.models import Order, OrderRequest, PlaceOrderResult
from susq.platform.prices import PIPS_PER_UNIT, to_pips

if TYPE_CHECKING:
    from susq_bridge.gateway import Gateway
    from susq_bridge.market_state import MarketState

__all__ = ["Oms", "TrackedOrder"]

log = logging.getLogger(__name__)


@dataclass(slots=True)
class TrackedOrder:
    order_id: int
    exchange_id: str
    side: str
    action: str
    yes_dir: int
    yes_price: int
    remaining: int
    expiration: datetime | None
    strategy: str = "unknown"
    intent_id: str = ""
    input_ts: float | None = None

    @classmethod
    def from_order(cls, order: Order) -> TrackedOrder:
        yes_dir = 1 if (order.side == "yes") == (order.action == "buy") else -1
        limit = to_pips(order.price_limit) if order.price_limit is not None else 0
        yes_price = limit if order.side == "yes" else PIPS_PER_UNIT - limit
        return cls(
            order_id=order.id,
            exchange_id=order.exchange_id,
            side=order.side,
            action=order.action,
            yes_dir=yes_dir,
            yes_price=yes_price,
            remaining=round(order.quantity),
            expiration=order.expiration_date,
        )


class Oms:
    def __init__(self, *, clock: Callable[[], datetime] = lambda: datetime.now(UTC)) -> None:
        self.clock = clock
        self.orders: dict[int, TrackedOrder] = {}
        self.positions: dict[str, int] = {}
        self.equity: float = 0.0
        self.last_reconcile: datetime | None = None
        self.reconciles = 0
        self._resync = asyncio.Event()
        self.resync_reasons: list[str] = []
        self.gateway: Gateway | None = None
        self.market: MarketState | None = None

    def attach(self, gateway: Gateway, market: MarketState) -> None:
        self.gateway = gateway
        self.market = market
        gateway.attach(self)

    # ── fast path (from our own responses) ──────────────────────────────────────────────────

    def on_placed(
        self,
        intent_id: str,
        strategy: str,
        request: OrderRequest,
        result: PlaceOrderResult,
        input_ts: float | None,
    ) -> None:
        if not result.open or result.order_id is None:
            return
        side = result.side or request.side
        action = result.action or request.action
        price = to_pips(result.price) if result.price is not None else request.price_pips
        yes_dir = 1 if (side == "yes") == (action == "buy") else -1
        self.orders[result.order_id] = TrackedOrder(
            order_id=result.order_id,
            exchange_id=request.exchange_id,
            side=side,
            action=action,
            yes_dir=yes_dir,
            yes_price=price if side == "yes" else PIPS_PER_UNIT - price,
            remaining=round(result.remaining_quantity or request.quantity),
            expiration=request.expiration,
            strategy=strategy,
            intent_id=intent_id,
            input_ts=input_ts,
        )

    def on_cancelled(self, order_id: int) -> None:
        self.orders.pop(order_id, None)

    def forget_all(self, exchange_id: str | None = None) -> None:
        for oid in [o.order_id for o in self.orders.values() if exchange_id in (None, o.exchange_id)]:
            self.orders.pop(oid, None)

    def open_orders(self, *, strategy: str | None = None) -> list[TrackedOrder]:
        now = self.clock()
        return [
            o
            for o in self.orders.values()
            if (o.expiration is None or o.expiration > now) and (strategy is None or o.strategy == strategy)
        ]

    # ── authoritative reconcile ─────────────────────────────────────────────────────────────

    def request_resync(self, reason: str, channel: str | None = None) -> None:
        """Realtime callback: something may have been missed; reconcile before acting."""
        self.resync_reasons.append(f"{reason}:{channel or '*'}")
        self._resync.set()

    async def reconcile(self) -> None:
        assert self.gateway is not None
        assert self.market is not None
        marks = self.market.marks()
        portfolio = await self.gateway.portfolio(marks)
        fresh: dict[int, TrackedOrder] = {}
        for order in portfolio.open_orders:
            tracked = TrackedOrder.from_order(order)
            known = self.orders.get(order.id)
            if known is not None:  # keep strategy metadata the platform doesn't know
                tracked.strategy, tracked.intent_id, tracked.input_ts = (
                    known.strategy,
                    known.intent_id,
                    known.input_ts,
                )
            fresh[order.id] = tracked
        self.orders = fresh
        self.positions = portfolio.positions
        self.equity = portfolio.equity
        self.gateway.risk.update_portfolio(
            portfolio.positions, portfolio.marks, portfolio.equity, portfolio.cash
        )
        self.last_reconcile = self.clock()
        self.reconciles += 1

    async def cancel_stale(self, max_age_s: float) -> list[int]:
        """Cancel orders whose decision inputs (fair values) are older than ``max_age_s``."""
        assert self.gateway is not None
        now = self.clock().timestamp()
        stale = [o for o in self.open_orders() if o.input_ts is not None and now - o.input_ts > max_age_s]
        cancelled = []
        for order in stale:
            if await self.gateway.cancel(order.order_id):
                cancelled.append(order.order_id)
        if cancelled:
            self.gateway.journal.event("stale_cancel", {"orders": cancelled, "max_age_s": max_age_s})
        return cancelled

    async def run(self, interval_s: float, stale_after_s: float) -> None:
        """Reconcile forever: on the interval, or at once when a resync is requested."""
        while True:
            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(self._resync.wait(), timeout=interval_s)
            self._resync.clear()
            try:
                await self.reconcile()
                await self.cancel_stale(stale_after_s)
            except Exception:
                log.exception("reconcile failed; will retry")
