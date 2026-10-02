"""Stale-order sniper: take resting orders that sit well through fair value.

A student bid above fair (or ask below fair) left behind after the real-money market moved is free
edge, *if* fair is right. So the sniper only ever acts when:

* the market has a **verified** MarketRule (a human has checked the Cup <-> reference mapping);
* the fair value is **fresh** (``fair.stale_after_s``) and **sane** (Cup mid within
  ``fair.mapping_error_pts`` of fair; otherwise it's probably an inverted or wrong mapping);
* the resting level is at least ``min_edge_pts`` through fair on the tick grid.

It takes exactly the stale levels (limit = the worst level still ``min_edge_pts`` through fair), never
walks into fair-priced liquidity, and sizes so that if fair were completely wrong the fill would lose
less than ``max_mistake_frac`` of bankroll. Per-market/cluster caps and the glitch cap are enforced by
the gateway's risk check; orders carry ``order_ttl_s`` and are cancelled when their fair input goes stale.
"""

from __future__ import annotations

import time
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime

from susq.platform.prices import PIPS_PER_UNIT, to_pips

from susq_bridge.config import SniperConfig
from susq_bridge.gateway import Gateway, OrderOutcome, OrderSpec
from susq_bridge.market_state import MarketState

__all__ = ["Sniper", "SniperStats", "Target"]


@dataclass(frozen=True, slots=True)
class Target:
    exchange_id: str
    side: str  # what we buy: "yes" lifts stale asks, "no" hits stale bids
    limit_pips: int  # side-relative
    quantity: int
    fair_pips: int
    edge_pips: int


@dataclass
class SniperStats:
    scans: int = 0
    candidates: int = 0
    orders: int = 0
    blocked: dict[str, int] = field(default_factory=dict)


class Sniper:
    def __init__(
        self,
        gateway: Gateway,
        market: MarketState,
        config: SniperConfig,
        *,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
    ) -> None:
        self.gateway = gateway
        self.market = market
        self.config = config
        self.clock = clock
        self.stats = SniperStats()
        self._last_action: dict[str, float] = {}

    def _block(self, reason: str) -> None:
        self.stats.blocked[reason] = self.stats.blocked.get(reason, 0) + 1

    async def find(self, exchange_id: str) -> Target | None:
        """A snipe on ``exchange_id``, or None (with the reason counted in ``stats.blocked``)."""
        check = self.market.fair_check(exchange_id)
        if not check.ok or check.fair_pips is None:
            self._block(check.reason)
            return None
        fair = check.fair_pips
        edge = round(self.config.min_edge_pts * 100)
        snap = self.market.prices.get(exchange_id)
        if snap is None:
            return None
        stale_ask = snap.best_ask is not None and to_pips(snap.best_ask) <= fair - edge
        stale_bid = snap.best_bid is not None and to_pips(snap.best_bid) >= fair + edge
        if not (stale_ask or stale_bid):
            return None
        book = await self.market.book(exchange_id, max_age_s=0.0)
        if stale_ask:
            levels = [
                (to_pips(lv.price), round(lv.quantity))
                for lv in book.asks
                if to_pips(lv.price) <= fair - edge
            ]
            side = "yes"
            limit = max((p for p, _ in levels), default=None)
        else:
            levels = [
                (to_pips(lv.price), round(lv.quantity))
                for lv in book.bids
                if to_pips(lv.price) >= fair + edge
            ]
            side = "no"
            worst_bid = min((p for p, _ in levels), default=None)
            limit = PIPS_PER_UNIT - worst_bid if worst_bid is not None else None
        depth = sum(q for _, q in levels)
        if limit is None or depth <= 0:
            return None  # the snapshot was already stale; the book has moved on
        edge_pips = (fair - limit) if side == "yes" else (limit - (PIPS_PER_UNIT - fair)) * -1
        # A wrong fair costs at most the price paid per share; keep that under max_mistake_frac.
        budget = self.config.max_mistake_frac * self.gateway.risk.equity * PIPS_PER_UNIT
        qty = min(depth, int(budget // limit))
        if qty < 1:
            self._block("size")
            return None
        return Target(exchange_id, side, limit, qty, fair, edge_pips)

    async def scan(self) -> list[OrderOutcome]:
        """One pass over every verified market with a fresh fair value."""
        self.stats.scans += 1
        self.market.load_fair()
        await self.market.refresh_prices()
        outcomes: list[OrderOutcome] = []
        if self.gateway.oms is not None:
            # The sniper's edge is a few points on near-certain outcomes held to settlement. Left
            # alone it fills every cap and starves the strategies that can actually win (far quotes,
            # the scenario book), so it stops adding once the book is this full.
            risk = self.gateway.risk
            total = risk.total_exposure_pips(self.gateway.oms.open_orders())
            if total >= self.config.max_total_gross_frac * risk.equity * PIPS_PER_UNIT:
                self._block("gross_budget")
                return []
        busy = (
            {o.exchange_id for o in self.gateway.oms.open_orders(strategy="sniper")}
            if self.gateway.oms
            else set()
        )
        for exchange_id in sorted(self.market.rules):
            if exchange_id in busy:
                continue
            if time.time() - self._last_action.get(exchange_id, 0.0) < self.config.order_ttl_s:
                continue
            target = await self.find(exchange_id)
            if target is None:
                continue
            self.stats.candidates += 1
            outcome = await self.place(target)
            outcomes.append(outcome)
        return outcomes

    async def place(self, target: Target) -> OrderOutcome:
        fair_ts = self.market.fair_produced_at.timestamp() if self.market.fair_produced_at else None
        self._last_action[target.exchange_id] = time.time()
        spec = OrderSpec(
            exchange_id=target.exchange_id,
            side=target.side,  # type: ignore[arg-type]
            action="buy",
            quantity=target.quantity,
            price_pips=target.limit_pips,
            ttl_s=self.config.order_ttl_s,
            fair_pips=target.fair_pips,
            input_ts=fair_ts,
        )
        stamp = int(self.clock().timestamp())
        outcome = await self.gateway.place(f"sniper:{target.exchange_id}:{stamp}", "sniper", spec)
        if outcome.placed:
            self.stats.orders += 1
        self.gateway.journal.event(
            "sniper_order",
            {
                "exchangeId": target.exchange_id,
                "side": target.side,
                "limit": target.limit_pips,
                "quantity": target.quantity,
                "fair": target.fair_pips,
                "edgePips": target.edge_pips,
                "status": outcome.status,
                "reason": outcome.reason,
            },
            idempotency_key=outcome.idempotency_key,
        )
        return outcome
