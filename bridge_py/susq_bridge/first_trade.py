"""The first qualifying trade: 1 share, near the mid, in the most liquid market (live mode only).

You must complete at least one trade to be ranked. This buys exactly one YES share:

1. pick the most liquid two-sided market: tightest spread, then deepest top of book;
2. if the spread is at most 2 ticks, pay the ask (it fills now); otherwise post a tick-rounded mid
   and wait up to ``wait_s`` for a fill, then cancel and pay the ask;
3. confirm the fill from ``GET /orders/{id}/fills`` and positions, and record fee and valuation
   observations into the DAY1 report.

It refuses to run in paper mode, without the lease, or when we already hold any position (already
ranked). The worst case is paying the ask on one share: less than 1 SUSQie.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import orjson
from susq.contracts import ops_pb2
from susq.platform.prices import TICK_PIPS, nearest_tick, to_pips, to_price

from susq_bridge.gateway import OrderSpec
from susq_bridge.measure_day1 import record_first_fill

if TYPE_CHECKING:
    from susq_bridge.runtime import Bridge

__all__ = ["FirstTrade", "first_trade", "pick_market"]


@dataclass(frozen=True, slots=True)
class FirstTrade:
    status: str  # filled | skipped | failed
    reason: str
    exchange_id: str | None = None
    order_id: int | None = None
    price: float | None = None


async def pick_market(bridge: Bridge, *, candidates: int = 10) -> tuple[str, int, int, float] | None:
    """(exchange, bid, ask, top depth) of the most liquid market, or None."""
    prices = await bridge.market.refresh_prices()
    two_sided = [
        (to_pips(p.best_ask) - to_pips(p.best_bid), ex)
        for ex, p in prices.items()
        if p.best_bid is not None and p.best_ask is not None and p.best_ask > p.best_bid
    ]
    best: tuple[tuple[int, float], tuple[str, int, int, float]] | None = None
    for spread, ex in sorted(two_sided)[:candidates]:
        book = await bridge.market.book(ex, depth=5, max_age_s=0.0)
        if not book.bids or not book.asks:
            continue
        depth = min(book.bids[0].quantity, book.asks[0].quantity)
        key = (spread, -depth)
        pick = (ex, to_pips(book.bids[0].price), to_pips(book.asks[0].price), depth)
        if best is None or key < best[0]:
            best = (key, pick)
    return best[1] if best is not None else None


async def first_trade(bridge: Bridge, *, wait_s: float = 60.0, poll_s: float = 2.0) -> FirstTrade:
    if bridge.mode != "live":
        return FirstTrade("skipped", "paper mode: the qualifying trade is live-only")
    if (why := await bridge.gateway.gate()) is not None:
        return FirstTrade("skipped", f"blocked: {why}")
    await bridge.oms.reconcile()
    if bridge.oms.positions:
        return FirstTrade("skipped", "already holding a position (already ranked)")
    picked = await pick_market(bridge)
    if picked is None:
        return FirstTrade("failed", "no two-sided market to trade")
    ex, bid, ask, _ = picked
    day = bridge.clock().date().isoformat()
    crossing = ask - bid <= 2 * TICK_PIPS
    price = ask if crossing else min(ask, nearest_tick((bid + ask) // 2))
    outcome = await bridge.gateway.place(
        _fresh_intent(bridge, f"first_trade:{day}:a"),
        "first_trade",
        OrderSpec(ex, "yes", "buy", 1, price, ttl_s=max(wait_s * 2, 120)),
    )
    if not outcome.placed or outcome.result is None:
        return FirstTrade("failed", f"{outcome.status}: {outcome.reason}", ex)
    result = outcome.result
    order_id = result.order_id
    if result.open and order_id is not None:
        filled = await _wait_for_fill(bridge, order_id, wait_s, poll_s)
        if not filled:
            await bridge.gateway.cancel(order_id)
            book = await bridge.market.book(ex, depth=1, max_age_s=0.0)
            if not book.asks:
                return FirstTrade("failed", "the mid didn't fill and the ask vanished", ex, order_id)
            ask = to_pips(book.asks[0].price)
            outcome = await bridge.gateway.place(
                _fresh_intent(bridge, f"first_trade:{day}:b"),
                "first_trade",
                OrderSpec(ex, "yes", "buy", 1, ask, ttl_s=120),
            )
            if not outcome.placed or outcome.result is None:
                return FirstTrade("failed", f"{outcome.status}: {outcome.reason}", ex)
            result, order_id, price = outcome.result, outcome.result.order_id, ask
            if (
                result.open
                and order_id is not None
                and not await _wait_for_fill(bridge, order_id, wait_s, poll_s)
            ):
                await bridge.gateway.cancel(order_id)
                return FirstTrade("failed", "no fill even at the ask", ex, order_id)
    fills = await bridge.gateway.read("order_fills", lambda: bridge.client.get_order_fills(order_id or 0))
    await bridge.oms.reconcile()
    fill_price = fills.avg_fill_price if fills.avg_fill_price is not None else to_price(price)
    positions = await bridge.gateway.read(
        "positions", lambda: bridge.client.get_positions(bridge.config.platform.tournament_slug)
    )
    pos = next((p for p in positions.positions if p.exchange_id == ex), None)
    book = await bridge.market.book(ex, depth=1, max_age_s=0.0)
    mid = (book.bids[0].price + book.asks[0].price) / 2 if book.bids and book.asks else None
    observation: dict[str, Any] = {
        "exchangeId": ex,
        "orderId": order_id,
        "quantity": 1,
        "price": fill_price,
        "totalCost": result.total_cost,
        "currentPrice": pos.current_price if pos else None,
        "midAfter": round(mid, 4) if mid is not None else None,
    }
    reports = bridge.config.reports_dir
    reports.mkdir(parents=True, exist_ok=True)
    (reports / "first_trade.json").write_bytes(orjson.dumps(observation, option=orjson.OPT_INDENT_2))
    record_first_fill(reports, observation)
    bridge.journal.event("first_trade", observation)
    return FirstTrade("filled", "ok", ex, order_id, fill_price)


def _fresh_intent(bridge: Bridge, base: str) -> str:
    """``base``, or ``base.N`` past attempts the platform rejected outright (e.g. TERMS_NOT_ACKNOWLEDGED
    before the rules were accepted). Anything that may have reached the book keeps its id, so a retry
    still dedupes against it."""
    intent_id, n = base, 0
    while (prior := bridge.gateway.intents.get(intent_id)) is not None and (
        prior.state == ops_pb2.INTENT_STATE_REJECTED
    ):
        n += 1
        intent_id = f"{base}.{n}"
    return intent_id


async def _wait_for_fill(bridge: Bridge, order_id: int, wait_s: float, poll_s: float) -> bool:
    waited = 0.0
    while waited < wait_s:
        order = await bridge.gateway.read("order", lambda: bridge.client.get_order(order_id))
        if not order.open:
            fills = await bridge.gateway.read("order_fills", lambda: bridge.client.get_order_fills(order_id))
            return abs(fills.total_quantity_filled) >= 1
        await asyncio.sleep(poll_s)
        waited += poll_s
    return False
