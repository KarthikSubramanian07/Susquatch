"""Far quoter: rests cheap ladders on near-certain markets, tops them up, pulls them when fair moves."""

from __future__ import annotations

from susq.platform.prices import PIPS_PER_UNIT


async def near_certain(harness, *, low: bool) -> str:  # type: ignore[no-untyped-def]
    fair = (await harness.mock.state())["fair"]
    picks = [ex for ex, p in fair.items() if (p <= 0.05 if low else p >= 0.95)]
    assert picks, "the mock should seed some near-certain markets"
    return sorted(picks)[0]


async def test_rests_a_no_ladder_on_a_near_zero_market_once(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.write_fair()
    ex = await near_certain(harness, low=True)
    bridge = await harness.open_bridge("live")
    try:
        cfg = bridge.config.farquote
        outcomes = await bridge.farquote.scan()
        mine = [o for o in outcomes if o.request and o.request.exchange_id == ex]
        assert len(mine) == len(cfg.levels)
        assert all(o.placed and o.request.side == "no" and o.request.action == "buy" for o in mine)
        assert sorted(o.request.price_pips for o in mine) == sorted(
            round(p * PIPS_PER_UNIT) for p in cfg.levels
        )
        resting = bridge.oms.open_orders(strategy="farquote")
        cost = sum(o.remaining * (PIPS_PER_UNIT - o.yes_price) for o in resting) / PIPS_PER_UNIT
        assert cost <= cfg.reserve_frac * bridge.gateway.risk.equity + 1e-6
        per_market = (
            sum(o.remaining * (PIPS_PER_UNIT - o.yes_price) for o in resting if o.exchange_id == ex)
            / PIPS_PER_UNIT
        )
        assert per_market <= cfg.max_market_frac * bridge.gateway.risk.equity + 1e-6
        # A second pass finds the ladder already resting and adds nothing.
        again = await bridge.farquote.scan()
        assert not [o for o in again if o.request and o.request.exchange_id == ex]
    finally:
        await bridge.close()


async def test_rests_a_yes_ladder_on_a_near_one_market(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    ex = "1313"  # the mock's strongest favourite (0.92): into the band, still inside the mapping check
    await harness.write_fair({ex: 0.96})
    bridge = await harness.open_bridge("live")
    try:
        outcomes = await bridge.farquote.scan()
        mine = [o for o in outcomes if o.request and o.request.exchange_id == ex]
        assert mine
        assert all(o.placed and o.request.side == "yes" for o in mine)
        assert max(o.request.price_pips for o in mine) <= 1000
    finally:
        await bridge.close()


async def test_pulls_the_ladder_when_fair_leaves_the_band(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.write_fair()
    ex = await near_certain(harness, low=True)
    bridge = await harness.open_bridge("live")
    try:
        await bridge.farquote.scan()
        assert [o for o in bridge.oms.open_orders(strategy="farquote") if o.exchange_id == ex]
        await harness.write_fair({ex: 0.5})
        await bridge.farquote.scan()
        assert not [o for o in bridge.oms.open_orders(strategy="farquote") if o.exchange_id == ex]
        assert bridge.farquote.stats.cancelled >= 1
    finally:
        await bridge.close()


async def test_leaves_ordinary_markets_alone(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.write_fair({"1309": 0.5})
    bridge = await harness.open_bridge("live")
    try:
        outcomes = await bridge.farquote.scan()
        assert not [o for o in outcomes if o.request and o.request.exchange_id == "1309"]
    finally:
        await bridge.close()


async def test_a_stale_fair_leaves_resting_ladders_alone(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.write_fair()
    ex = await near_certain(harness, low=True)
    bridge = await harness.open_bridge("live")
    try:
        await bridge.farquote.scan()
        before = len([o for o in bridge.oms.open_orders(strategy="farquote") if o.exchange_id == ex])
        assert before
        await harness.write_fair(age_s=harness.config.fair.stale_after_s + 1)
        await bridge.farquote.scan()
        after = len([o for o in bridge.oms.open_orders(strategy="farquote") if o.exchange_id == ex])
        assert after == before
        assert bridge.farquote.stats.cancelled == 0
    finally:
        await bridge.close()
