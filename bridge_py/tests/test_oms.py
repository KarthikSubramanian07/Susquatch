"""OMS: reconcile from REST, keep strategy metadata, resync on demand."""

from __future__ import annotations

import asyncio
import contextlib

import pytest
from susq_bridge.gateway import OrderSpec

EX = "1309"


async def test_reconcile_adopts_orders_it_did_not_place(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    foreign = await harness.mock.client.place_order(harness.mock.order(EX, price_pips=bid - 300), "outside-1")
    assert live_bridge.oms.open_orders() == []
    await live_bridge.oms.reconcile()
    adopted = live_bridge.oms.open_orders()
    assert [o.order_id for o in adopted] == [foreign.order_id]
    assert adopted[0].strategy == "unknown"
    assert adopted[0].yes_dir == 1


async def test_reconcile_keeps_strategy_metadata_and_drops_closed(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    out = await live_bridge.gateway.place(
        "t:meta", "sniper", OrderSpec(EX, "yes", "buy", 5, bid - 200, input_ts=1.0)
    )
    await live_bridge.oms.reconcile()
    assert live_bridge.oms.open_orders()[0].strategy == "sniper"
    await harness.mock.client.cancel_order(out.result.order_id)  # cancelled behind our back
    await live_bridge.oms.reconcile()
    assert live_bridge.oms.open_orders() == []


async def test_reconcile_updates_positions_equity_and_risk(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    _, ask = await harness.top(EX)
    await live_bridge.gateway.place("t:fill", "test", OrderSpec(EX, "yes", "buy", 5, ask))
    await live_bridge.oms.reconcile()
    assert live_bridge.oms.positions == {EX: 5}
    assert live_bridge.risk.positions == {EX: 5}
    assert live_bridge.oms.equity == pytest.approx(100_000, abs=1)


async def test_resync_request_triggers_an_immediate_reconcile(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    before = live_bridge.oms.reconciles
    task = asyncio.create_task(live_bridge.oms.run(interval_s=3600, stale_after_s=60))
    try:
        live_bridge.oms.request_resync("gap", "tournament:x")
        for _ in range(100):
            if live_bridge.oms.reconciles > before:
                break
            await asyncio.sleep(0.01)
        assert live_bridge.oms.reconciles == before + 1
        assert live_bridge.oms.resync_reasons == ["gap:tournament:x"]
    finally:
        task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await task
