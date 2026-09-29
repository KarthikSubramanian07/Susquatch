"""Constraints racer: the payoff proof, bundle sizing, and trading violations end to end."""

from __future__ import annotations

import asyncio
import contextlib
import time
from pathlib import Path

import httpx
from hypothesis import given
from hypothesis import strategies as st
from susq.mock import MockConfig
from susq.mock.testing import KEY, serve_mock
from susq.platform.models import BookLevel, ConstraintEvaluation, OrderBook
from susq_bridge.config import load_config
from susq_bridge.runtime import Bridge, Loops
from susq_bridge.strategies.racer import Leg, allowed_outcomes, build_bundle, guaranteed_payoff, ladder

NO_EDGE = 100  # 0.01


def book(bids: list[tuple[float, float]] = (), asks: list[tuple[float, float]] = ()) -> OrderBook:  # type: ignore[assignment]
    return OrderBook(
        exchange_id="x",
        bids=[BookLevel(price=p, quantity=q) for p, q in bids],
        asks=[BookLevel(price=p, quantity=q) for p, q in asks],
    )


# ── the proof ───────────────────────────────────────────────────────────────────────────────


def test_no_bundle_on_mutually_exclusive_pays_n_minus_one_even_if_nobody_wins() -> None:
    outcomes = allowed_outcomes("mutually_exclusive", ["a", "b", "c"], none_possible=True)
    assert guaranteed_payoff({"a": "no", "b": "no", "c": "no"}, outcomes) == 20_000


def test_yes_bundle_is_worthless_when_nobody_can_win() -> None:
    risky = allowed_outcomes("mutually_exclusive", ["a", "b"], none_possible=True)
    safe = allowed_outcomes("mutually_exclusive", ["a", "b"], none_possible=False)
    assert guaranteed_payoff({"a": "yes", "b": "yes"}, risky) == 0
    assert guaranteed_payoff({"a": "yes", "b": "yes"}, safe) == 10_000


def test_implication_bundle_pays_at_least_one() -> None:
    outcomes = allowed_outcomes("implication", ["a", "c"], none_possible=True, antecedent="a")
    assert {tuple(sorted(o.items())) for o in outcomes} == {
        (("a", 1), ("c", 1)),
        (("a", 0), ("c", 1)),
        (("a", 0), ("c", 0)),
    }
    assert guaranteed_payoff({"a": "no", "c": "yes"}, outcomes) == 10_000


def test_ladder_complements_bids_for_no() -> None:
    b = book(bids=[(0.60, 5), (0.58, 7)], asks=[(0.62, 3)])
    assert ladder(b, "no") == ((4000, 5), (4200, 7))
    assert ladder(b, "yes") == ((6200, 3),)


def test_build_bundle_walks_depth_while_edge_holds() -> None:
    outcomes = allowed_outcomes("mutually_exclusive", ["a", "b"], none_possible=True)
    legs = [
        Leg("a", "no", ((4000, 5), (4300, 100))),  # bids 0.60 x5 then 0.57
        Leg("b", "no", ((5400, 10), (5600, 100))),  # bids 0.46 x10 then 0.44
    ]
    bundle, reason = build_bundle(
        "r", "mutually_exclusive", legs, outcomes, min_edge_pips=NO_EDGE, max_notional_pips=10**9
    )
    assert reason == "ok"
    assert bundle is not None
    # q=5: 0.40 + 0.54 = 0.94 (edge 0.06); q=10: 0.43 + 0.54 = 0.97 (edge 0.03); q=105: 0.43+0.56=0.99 (0.01)
    assert bundle.quantity == 105
    assert bundle.limits == (4300, 5600)
    assert bundle.edge_pips == 100


def test_build_bundle_respects_notional_cap() -> None:
    outcomes = allowed_outcomes("mutually_exclusive", ["a", "b"], none_possible=True)
    legs = [Leg("a", "no", ((4000, 1000),)), Leg("b", "no", ((5400, 1000),))]
    bundle, _ = build_bundle(
        "r", "mutually_exclusive", legs, outcomes, min_edge_pips=NO_EDGE, max_notional_pips=9_400 * 7
    )
    assert bundle is not None
    assert bundle.quantity == 7


def test_build_bundle_refuses_without_edge_or_depth() -> None:
    outcomes = allowed_outcomes("mutually_exclusive", ["a", "b"], none_possible=True)
    flat = [Leg("a", "no", ((5000, 10),)), Leg("b", "no", ((5000, 10),))]
    assert build_bundle("r", "me", flat, outcomes, min_edge_pips=NO_EDGE, max_notional_pips=10**9) == (
        None,
        "no_edge",
    )
    empty = [Leg("a", "no", ()), Leg("b", "no", ((5000, 10),))]
    assert (
        build_bundle("r", "me", empty, outcomes, min_edge_pips=NO_EDGE, max_notional_pips=10**9)[1]
        == "no_depth"
    )


@given(
    prices=st.lists(st.integers(min_value=1, max_value=199).map(lambda t: t * 50), min_size=2, max_size=4),
    none_possible=st.booleans(),
    side=st.sampled_from(["yes", "no"]),
)
def test_any_accepted_bundle_is_profitable_in_every_allowed_outcome(
    prices: list[int], none_possible: bool, side: str
) -> None:
    ids = [f"e{i}" for i in range(len(prices))]
    outcomes = allowed_outcomes("mutually_exclusive", ids, none_possible=none_possible)
    legs = [Leg(e, side, ((p, 10),)) for e, p in zip(ids, prices, strict=True)]
    bundle, _ = build_bundle(
        "r", "mutually_exclusive", legs, outcomes, min_edge_pips=NO_EDGE, max_notional_pips=10**9
    )
    if bundle is None:
        return
    for outcome in outcomes:
        payoff = sum(10_000 * (outcome[e] if side == "yes" else 1 - outcome[e]) for e in ids)
        assert payoff - bundle.cost_pips >= NO_EDGE


# ── against the mock ────────────────────────────────────────────────────────────────────────


def violated(rel_id: str, tid: str, kind: str = "mutually_exclusive") -> ConstraintEvaluation:
    return ConstraintEvaluation(
        relationship_id=rel_id,
        type=kind,
        evaluation_status="violated",
        violation_amount=0.05,
        tournament_id=tid,
    )


async def rel_id_for(harness, exchange_id: str) -> str:  # type: ignore[no-untyped-def]
    state = await harness.mock.state()
    return next(r["id"] for r in state["relationships"] if exchange_id in r["exchangeIds"])


async def test_racer_trades_a_sum_above_one_violation(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("live")
    try:
        out = await harness.mock.post_admin("/_mock/violation", {"amount": 0.06})
        await bridge.racer.load_relationships()
        outcomes = await bridge.racer.poll_once()
        assert len(outcomes) == 1
        bundle = outcomes[0]
        assert bundle.placed
        assert {r.side for r in bundle.requests} == {"no"}
        rel = bridge.racer.relationships[out["relationshipId"]]
        positions = {
            p.exchange_id: p.quantity
            for p in (await harness.mock.client.get_positions("midterm-elections")).positions
        }
        legs = rel.exchange_ids()
        assert positions[legs[0]] == positions[legs[1]] < 0  # equal NO on both legs
        cost = sum(r.total_cost for r in bundle.results) / -positions[legs[0]]
        assert cost <= 1 - bridge.config.racer.min_edge  # guaranteed payoff is 1 per bundle share
        assert await harness.mock.client.list_open_orders(harness.mock.tid) == []  # remainders cancelled
        assert bridge.racer.stats.bundles == 1
        assert bridge.racer.stats.latencies_ms
        assert await bridge.racer.poll_once() == []  # cooldown: the same violation isn't traded twice
    finally:
        await bridge.close()


async def test_racer_stops_accumulating_at_the_relationship_exposure_cap(harness) -> None:  # type: ignore[no-untyped-def]
    """Regression: a standing violation must not be re-traded without bound, one cooldown at a time."""
    import dataclasses

    await harness.write_market_map()
    bridge = await harness.open_bridge("live")
    try:
        await harness.mock.post_admin("/_mock/violation", {"amount": 0.06})
        await bridge.racer.load_relationships()
        first = await bridge.racer.poll_once()
        assert len(first) == 1
        assert first[0].placed
        await bridge.oms.reconcile()  # risk now sees the NO positions on both legs

        # Cooldown elapsed and the violation is re-created, but the cap is already used up by what we hold.
        bridge.racer._last_action.clear()
        held = sum(abs(bridge.gateway.risk.position_value_pips(r.exchange_id)) for r in first[0].requests)
        cap_frac = held / (bridge.gateway.risk.equity * 10_000)  # exactly what we already hold
        bridge.racer.config = dataclasses.replace(
            bridge.racer.config, max_relationship_notional_frac=cap_frac
        )
        await harness.mock.post_admin("/_mock/violation", {"amount": 0.06})
        assert await bridge.racer.poll_once() == []
        assert bridge.racer.stats.bundles == 1
    finally:
        await bridge.close()


async def test_racer_refuses_unsafe_yes_bundle_when_an_independent_can_win(harness) -> None:  # type: ignore[no-untyped-def]
    tx_d, tx_r = "1307", "1308"
    await harness.mock.post_admin("/_mock/fair", {"exchangeId": tx_d, "fair": 0.40})
    await harness.mock.post_admin("/_mock/fair", {"exchangeId": tx_r, "fair": 0.45})  # asks sum ~0.87
    await harness.write_market_map(independent={tx_d, tx_r})
    bridge = await harness.open_bridge("live")
    try:
        rel = await rel_id_for(harness, tx_d)
        assert await bridge.racer.handle(violated(rel, bridge.tournament.id), time.time()) is None
        assert bridge.racer.stats.unsafe == 1
        assert await harness.mock.client.list_open_orders(harness.mock.tid) == []
        assert (await harness.mock.client.get_positions("midterm-elections")).positions == []

        # Same books, but now a human has verified that nobody else can win: the proof goes through.
        await harness.write_market_map(independent=set())
        bridge.market.load_market_map()
        outcome = await bridge.racer.handle(violated(rel, bridge.tournament.id), time.time())
        assert outcome is not None
        assert outcome.placed
        assert {r.side for r in outcome.requests} == {"yes"}
    finally:
        await bridge.close()


async def test_racer_refuses_yes_bundle_on_non_exhaustive_relationship(harness) -> None:  # type: ignore[no-untyped-def]
    ne_d, ne_r = "1315", "1316"  # Nebraska: engine marks the pair non-exhaustive
    await harness.mock.post_admin("/_mock/fair", {"exchangeId": ne_d, "fair": 0.10})
    await harness.mock.post_admin("/_mock/fair", {"exchangeId": ne_r, "fair": 0.40})
    await harness.write_market_map(independent=set())  # even with (wrongly) verified rules
    bridge = await harness.open_bridge("live")
    try:
        rel = await rel_id_for(harness, ne_d)
        assert await bridge.racer.handle(violated(rel, bridge.tournament.id), time.time()) is None
        assert bridge.racer.stats.unsafe == 1
    finally:
        await bridge.close()


async def test_racer_trades_an_implication_violation(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("live")
    try:
        await bridge.racer.load_relationships()
        rel = next(r for r in bridge.racer.relationships.values() if r.type == "implication")
        await harness.mock.post_admin("/_mock/violation", {"relationshipId": rel.id, "amount": 0.06})
        outcome = await bridge.racer.handle(
            violated(rel.id, bridge.tournament.id, "implication"), time.time()
        )
        assert outcome is not None
        assert outcome.placed
        sides = {r.exchange_id: r.side for r in outcome.requests}
        assert sides == {rel.exchange_ids("antecedent")[0]: "no", rel.exchange_ids("consequent")[0]: "yes"}
    finally:
        await bridge.close()


async def test_racer_ignores_other_tournaments_and_satisfied_rows(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("live")
    try:
        assert await bridge.racer.handle(violated("r", "someone-else"), time.time()) is None
        ok = ConstraintEvaluation(
            relationship_id="r", type="mutually_exclusive", evaluation_status="satisfied"
        )
        assert await bridge.racer.handle(ok, time.time()) is None
        assert bridge.racer.stats.violations == 0
    finally:
        await bridge.close()


async def test_racer_in_paper_mode_trades_nothing_real(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("paper")
    try:
        await harness.mock.post_admin("/_mock/violation", {"amount": 0.06})
        outcomes = await bridge.racer.poll_once()
        assert outcomes[0].placed
        assert (await harness.mock.client.get_positions("midterm-elections")).positions == []
        assert any(q < 0 for q in (h.quantity for h in bridge.gateway.paper.ledger.holdings.values()))
    finally:
        await bridge.close()


async def test_realtime_violation_is_raced(home: Path) -> None:
    """Mock on a real socket: the bridge joins realtime and races the pushed violation."""
    (home / "ops").mkdir()
    (home / "ops" / "GO_LIVE").write_text("go live")
    async with serve_mock(MockConfig(seed=7)) as live:
        config = load_config(home, env={"SUSQ_API_KEY": KEY, "SUSQ_MODE": "live", "SUSQ_BASE_URL": live.api})
        bridge = await Bridge.open(config)
        await bridge.start()
        runner = asyncio.create_task(bridge.run(Loops(sniper=False, recorder=False, status_interval_s=3600)))
        try:
            for _ in range(200):
                if bridge.realtime is not None and bridge.realtime.connected.is_set():
                    break
                await asyncio.sleep(0.02)
            assert bridge.realtime is not None
            assert bridge.realtime.connected.is_set()
            async with httpx.AsyncClient(base_url=live.base) as admin:
                await admin.post("/_mock/violation", json={"amount": 0.06})
            deadline = time.monotonic() + 10
            while bridge.racer.stats.bundles == 0 and time.monotonic() < deadline:
                await asyncio.sleep(0.02)
            assert bridge.racer.stats.bundles == 1
            assert bridge.racer.stats.latencies_ms[0] < 5_000
        finally:
            runner.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await runner
            await bridge.close()
