"""Sniper: takes stale levels through a fresh, sane, verified fair; refuses everything else."""

from __future__ import annotations

from dataclasses import replace

EX = "1309"  # Michigan D (clean book at seed 7)


async def positions(harness) -> dict[str, float]:  # type: ignore[no-untyped-def]
    res = await harness.mock.client.get_positions("midterm-elections")
    return {p.exchange_id: p.quantity for p in res.positions}


async def test_takes_a_stale_ask_below_fair(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.mock.post_admin(
        "/_mock/stale-order", {"exchangeId": EX, "side": "ask", "through": 0.05, "quantity": 40}
    )
    await harness.write_fair()
    bridge = await harness.open_bridge("live")
    try:
        outcomes = await bridge.sniper.scan()
        mine = [o for o in outcomes if o.request and o.request.exchange_id == EX]
        assert len(mine) == 1
        assert mine[0].placed
        assert mine[0].request.side == "yes"
        fair = await harness.fair(EX)
        assert fair - mine[0].request.price_pips >= bridge.config.sniper.min_edge_pts * 100
        assert (await positions(harness))[EX] == 40
    finally:
        await bridge.close()


async def test_hits_a_stale_bid_above_fair_by_buying_no(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.mock.post_admin(
        "/_mock/stale-order", {"exchangeId": EX, "side": "bid", "through": 0.06, "quantity": 25}
    )
    await harness.write_fair()
    bridge = await harness.open_bridge("live")
    try:
        outcomes = await bridge.sniper.scan()
        mine = [o for o in outcomes if o.request and o.request.exchange_id == EX]
        assert mine[0].placed
        assert mine[0].request.side == "no"
        assert (await positions(harness))[EX] == -25
    finally:
        await bridge.close()


async def test_refuses_a_stale_fair(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.mock.post_admin("/_mock/stale-order", {"exchangeId": EX, "side": "ask", "through": 0.05})
    await harness.write_fair(age_s=harness.config.fair.stale_after_s + 1)
    bridge = await harness.open_bridge("live")
    try:
        assert await bridge.sniper.scan() == []
        assert bridge.sniper.stats.blocked["stale"] >= 1
        assert await positions(harness) == {}
    finally:
        await bridge.close()


async def test_refuses_and_flags_a_mapping_error(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.mock.post_admin("/_mock/stale-order", {"exchangeId": EX, "side": "ask", "through": 0.05})
    await harness.write_fair({EX: 0.05})  # an inverted D/R mapping would look exactly like this
    bridge = await harness.open_bridge("live")
    try:
        outcomes = await bridge.sniper.scan()
        assert all(o.request is None or o.request.exchange_id != EX for o in outcomes)
        assert bridge.sniper.stats.blocked["mapping_error"] >= 1
        assert EX in bridge.market.flagged
        status = bridge.status.build(bridge)
        assert any(EX in e for e in status.events)
    finally:
        await bridge.close()


async def test_refuses_unverified_markets(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map(verified=False)
    await harness.mock.post_admin("/_mock/stale-order", {"exchangeId": EX, "side": "ask", "through": 0.05})
    await harness.write_fair()
    bridge = await harness.open_bridge("live")
    try:
        assert await bridge.sniper.scan() == []
        assert bridge.sniper.stats.blocked["unverified"] >= 1
    finally:
        await bridge.close()


async def test_size_keeps_a_wrong_fill_under_the_mistake_budget(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.mock.post_admin(
        "/_mock/stale-order", {"exchangeId": EX, "side": "ask", "through": 0.05, "quantity": 500}
    )
    await harness.write_fair()
    bridge = await harness.open_bridge("live")
    try:
        bridge.sniper.config = replace(bridge.sniper.config, max_mistake_frac=0.0001)  # 10 SUSQies
        outcomes = await bridge.sniper.scan()
        mine = next(o for o in outcomes if o.request and o.request.exchange_id == EX)
        req = mine.request
        assert req.quantity * req.price_pips / 10_000 <= 0.0001 * bridge.risk.equity
    finally:
        await bridge.close()


async def test_sniper_orders_are_cancelled_when_fair_goes_stale(harness) -> None:  # type: ignore[no-untyped-def]
    from susq_bridge.gateway import OrderSpec

    bridge = await harness.open_bridge("live")
    try:
        bid, _ = await harness.top(EX)
        old = harness.now().timestamp() - 3600
        out = await bridge.gateway.place(
            "t:stale-input", "sniper", OrderSpec(EX, "yes", "buy", 5, bid - 200, input_ts=old)
        )
        assert out.placed
        cancelled = await bridge.oms.cancel_stale(bridge.config.fair.stale_after_s)
        assert cancelled == [out.result.order_id]
        assert await harness.mock.client.list_open_orders(harness.mock.tid) == []
    finally:
        await bridge.close()


async def test_reads_the_fair_file_research_writes(harness) -> None:  # type: ignore[no-untyped-def]
    """Interop: the research FairService's own writer feeds the bridge's staleness + mapping gates."""
    from susq.contracts import targets_pb2, to_json
    from susq.fair.service import fair_file

    await harness.write_market_map()
    state = await harness.mock.state()
    fairs = [
        targets_pb2.FairValue(exchange_id=ex, p_yes=p, source="research") for ex, p in state["fair"].items()
    ]
    tf = fair_file(fairs, seq=1, now_ns=int(harness.now().timestamp() * 1e9))
    harness.config.fair_path.parent.mkdir(parents=True, exist_ok=True)
    harness.config.fair_path.write_text(to_json(tf))
    bridge = await harness.open_bridge("paper")
    try:
        assert bridge.market.load_fair() is False  # already loaded by start()
        check = bridge.market.fair_check(EX)
        assert check.ok, check.reason
        assert check.fair_pips == round(state["fair"][EX] * 10_000)
    finally:
        await bridge.close()
