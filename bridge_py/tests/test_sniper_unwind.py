"""EXP-003: the sniper unwinds its book from ``unwind_from_utc`` (covered sells near fair, never below
fair - haircut, never a locked leg, stopping at the cash target) and buys nothing after that."""

from __future__ import annotations

import json
from dataclasses import replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from susq.calibrate.targets import build_target_file, write_targets
from susq.contracts import targets_pb2
from susq.platform.prices import PIPS_PER_UNIT
from susq_bridge.config import SniperConfig, load_config
from susq_bridge.strategies.sniper import UnwindPlan, plan_unwind

FIXTURE = Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "sniper_unwind.json"
EX = "1309"  # Michigan D (clean book at seed 7)
START = datetime(2026, 10, 24, 16, 0, tzinfo=UTC)
CFG = SniperConfig(
    enabled=True, min_edge_pts=3.5, max_mistake_frac=0.01, order_ttl_s=30, max_total_gross_frac=0.35
)


def plan(config: SniperConfig = CFG, **kw: Any) -> UnwindPlan:
    args: dict[str, Any] = {
        "now": START,
        "equity": 100_000.0,
        "cash": 20_000.0,
        "hourly_used": 0.0,
        "positions": {"10": 500},
        "fair_pips": {"10": 9500},
        "tops": {"10": (9400, 9600)},
    }
    args.update(kw)
    return plan_unwind(config, **args)


def sells(p: UnwindPlan) -> list[tuple[str, str, int, int]]:
    return [(o.exchange_id, o.side, o.quantity, o.price_pips) for o in p.orders]


# ── the rule (pure) ─────────────────────────────────────────────────────────────────────────


def test_shipped_config_equals_the_defaults(home: Path) -> None:
    cfg = load_config(home, env={}).sniper
    assert cfg.unwind_from_utc == START
    assert cfg == CFG, "config/susq.toml spells out the same values the dataclass defaults to"
    fixture = json.loads(FIXTURE.read_text())["config"]
    assert fixture["unwind_from_utc"] == "2026-10-24T16:00:00Z"
    for key in (
        "max_mistake_frac",
        "unwind_max_haircut_pts",
        "unwind_target_cash_frac",
        "unwind_max_hourly_frac",
    ):
        assert fixture[key] == getattr(cfg, key), key


@pytest.mark.parametrize(
    ("line", "bad"),
    [
        ("unwind_max_haircut_pts = 2.0", "unwind_max_haircut_pts = -1.0"),
        ("unwind_target_cash_frac = 0.60", "unwind_target_cash_frac = 1.5"),
        ("unwind_max_hourly_frac = 0.10", "unwind_max_hourly_frac = -0.1"),
        ('unwind_from_utc = "2026-10-24T16:00:00Z"', 'unwind_from_utc = "2026-10-24T16:00:00"'),
    ],
)
def test_unsafe_unwind_values_are_refused(home: Path, line: str, bad: str) -> None:
    toml = home / "config" / "susq.toml"
    text = toml.read_text()
    assert line in text
    toml.write_text(text.replace(line, bad, 1))
    with pytest.raises(ValueError, match=r"unwind|timezone"):
        load_config(home, env={})


def test_no_unwind_before_the_start() -> None:
    p = plan(now=START - timedelta(seconds=1))
    assert p.orders == []
    assert p.stop == "not_started"
    assert plan(config=replace(CFG, unwind_from_utc=None)).stop == "not_started"
    assert sells(plan(now=START)) == [("10", "yes", 500, 9400)]


def test_sells_at_the_better_of_bid_and_fair_minus_haircut() -> None:
    # Bid 0.94 is within 2 pts of fair 0.95: take it.
    assert sells(plan(tops={"10": (9400, 9600)})) == [("10", "yes", 500, 9400)]
    # Bid 0.96 above fair: still the bid.
    assert sells(plan(tops={"10": (9600, 9700)})) == [("10", "yes", 500, 9600)]
    # Bid 0.92 deeper than the haircut: rest at the floor 0.93 instead.
    assert sells(plan(tops={"10": (9200, 9600)})) == [("10", "yes", 500, 9300)]
    # No bid at all: rest at the floor.
    assert sells(plan(tops={"10": (None, 9600)})) == [("10", "yes", 500, 9300)]
    # Floor rounds to the tick toward fair: 0.9537 - 0.02 = 0.9337 -> 0.935.
    assert sells(plan(fair_pips={"10": 9537}, tops={"10": (9000, 9600)})) == [("10", "yes", 500, 9350)]
    # A NO holding sells NO: the NO bid is 1 - YES ask.
    p = plan(positions={"20": -400}, fair_pips={"20": 400}, tops={"20": (300, 500)})
    assert sells(p) == [("20", "no", 400, 9500)]


@pytest.mark.parametrize("fair", [9000, 9123, 9480, 9537, 9990])
@pytest.mark.parametrize("bid", [None, 8000, 8900, 9250, 9450, 9950])
def test_never_below_fair_minus_haircut(fair: int, bid: int | None) -> None:
    for haircut in (0.0, 0.5, 2.0, 5.0):
        cfg = replace(CFG, unwind_max_haircut_pts=haircut)
        p = plan(config=cfg, fair_pips={"10": fair}, tops={"10": (bid, None)})
        floor = fair - haircut * 100
        if floor > 9950 and (bid is None or bid < floor):
            # No valid price at or above the floor (0.995 is the top): nothing is sold.
            assert p.orders == []
            assert p.skipped == {"10": "no_valid_price"}
            continue
        (order,) = p.orders
        assert order.price_pips >= fair - haircut * 100
        assert order.price_pips % 50 == 0
        assert order.price_pips == max(order.floor_pips, bid or 0)


def test_never_sells_more_than_held_and_never_flips() -> None:
    assert sells(plan(positions={"10": 3})) == [("10", "yes", 3, 9400)]
    # A big holding goes one per-order cap (1% of equity in proceeds) at a time.
    (order,) = plan(positions={"10": 50_000}).orders
    assert order.quantity == 1063
    assert order.quantity * order.price_pips <= 0.01 * 100_000 * PIPS_PER_UNIT
    assert plan(positions={"10": 0}).orders == []


def test_never_touches_locked_legs_or_other_strategies_holdings() -> None:
    assert plan(locked={"10"}).orders == []
    assert plan(locked={"10"}).skipped == {"10": "locked"}
    assert plan(other_strategy={"10"}).skipped == {"10": "other_strategy"}
    assert plan(busy={"10"}).skipped == {"10": "busy"}
    # Longshot holdings (a scenario-style cheap leg) and mid-priced ones are not the sniper's book.
    assert plan(positions={"10": -500}).skipped == {"10": "not_near_certain"}
    assert plan(fair_pips={"10": 8950}).skipped == {"10": "not_near_certain"}
    assert plan(fair_pips={}).skipped == {"10": "no_fair"}


def test_stops_at_the_cash_target() -> None:
    assert plan(cash=60_000.0).stop == "cash_target"
    assert plan(cash=75_000.0).orders == []
    # The last sell only covers the gap: 500 SUSQies short at 0.94 is 532 shares, not 1063.
    p = plan(cash=59_500.0, positions={"10": 5_000})
    assert sells(p) == [("10", "yes", 532, 9400)]
    assert p.stop == "cash_target"
    assert plan(cash=None).stop == "no_cash"


def test_hourly_cap() -> None:
    p = plan(hourly_used=10_000.0)
    assert p.orders == []
    assert p.stop == "hourly_cap"
    assert sells(plan(hourly_used=9_900.0)) == [("10", "yes", 106, 9400)]


def test_parity_fixture_shared_with_rust() -> None:
    """``tests/fixtures/sniper_unwind.json`` is also run through rust-live's sniper
    (``crates/strategies/src/sniper.rs``): both traders must emit exactly these sells."""
    fixture = json.loads(FIXTURE.read_text())
    raw = fixture["config"]
    cfg = replace(
        CFG,
        max_mistake_frac=raw["max_mistake_frac"],
        unwind_from_utc=datetime.fromisoformat(raw["unwind_from_utc"].replace("Z", "+00:00")),
        unwind_max_haircut_pts=raw["unwind_max_haircut_pts"],
        unwind_target_cash_frac=raw["unwind_target_cash_frac"],
        unwind_max_hourly_frac=raw["unwind_max_hourly_frac"],
    )
    assert fixture["cases"]
    for case in fixture["cases"]:
        intents = case["intents"]
        p = plan_unwind(
            cfg,
            now=datetime.fromtimestamp(case["now_s"], UTC),
            equity=case["equity"],
            cash=case["cash"],
            hourly_used=case["hourly_used"],
            positions=case["positions"],
            fair_pips={ex: round(f * PIPS_PER_UNIT) for ex, f in case["fair"].items()},
            tops={ex: (b[0], b[1]) for ex, b in case["books"].items()},
            locked=case["locked"],
            other_strategy={i["exchange"] for i in intents if i["strategy"] != "sniper"},
            busy={i["exchange"] for i in intents if i["strategy"] == "sniper"},
        )
        want = [(e["exchange"], e["side"], e["quantity"], e["price"]) for e in case["expect"]]
        assert sells(p) == want, case["name"]


# ── the bridge, end to end on the mock ──────────────────────────────────────────────────────


async def positions(harness) -> dict[str, float]:  # type: ignore[no-untyped-def]
    res = await harness.mock.client.get_positions("midterm-elections")
    return {p.exchange_id: p.quantity for p in res.positions}


async def hold_a_snipe(harness, bridge) -> None:  # type: ignore[no-untyped-def]
    """Re-mark EX to 0.95, let the sniper take a stale ask at 0.90, and reconcile the portfolio."""
    await harness.mock.post_admin("/_mock/fair", {"exchangeId": EX, "fair": 0.95})
    await harness.mock.post_admin(
        "/_mock/stale-order", {"exchangeId": EX, "side": "ask", "through": 0.05, "quantity": 40}
    )
    await harness.write_fair()
    bridge.sniper.config = replace(bridge.sniper.config, unwind_from_utc=harness.now() + timedelta(days=1))
    outcomes = await bridge.sniper.scan()
    assert any(o.placed and o.request.exchange_id == EX for o in outcomes)
    assert (await positions(harness))[EX] == 40
    await bridge.oms.reconcile()
    assert bridge.risk.positions[EX] == 40


def journal_events(bridge, kind: str) -> list[dict[str, Any]]:  # type: ignore[no-untyped-def]
    bridge.journal.flush()
    out = []
    for path in sorted(bridge.config.journal_dir.rglob("bridge.journal.jsonl")):
        for line in path.read_text().splitlines():
            entry = json.loads(line) if line.strip() else {}
            if entry.get("method") == kind:
                out.append(json.loads(entry.get("bodyJson") or entry["body_json"]))
    return out


async def test_unwinds_a_held_snipe_near_fair_after_the_start(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("live")
    try:
        await hold_a_snipe(harness, bridge)
        # Before the start nothing is sold, however long it is held.
        assert all(o.request.action == "buy" for o in await bridge.sniper.scan() if o.request)
        bridge.sniper.config = replace(bridge.sniper.config, unwind_from_utc=harness.now())
        bridge.sniper._last_action.clear()
        bridge.risk.cash = 1_000.0  # far below 60% of equity, so the whole holding may go
        await harness.write_fair()
        outcomes = await bridge.sniper.scan()
        (sell,) = [o for o in outcomes if o.request and o.request.exchange_id == EX]
        req = sell.request
        assert (req.side, req.action, req.quantity) == ("yes", "sell", 40)
        fair = await harness.fair(EX)
        assert req.price_pips >= fair - 200, "never below fair - 2 pts"
        assert sell.placed
        (event,) = [e for e in journal_events(bridge, "sniper_unwind") if e["exchangeId"] == EX]
        assert event["status"] == "placed"
        assert event["quantity"] == 40
        assert event["haircutPips"] <= 200
        assert event["limit"] >= event["floor"]
        assert (await positions(harness)).get(EX, 0) >= 0, "a covered sell never flips the holding short"
    finally:
        await bridge.close()


async def test_no_new_sniper_buys_after_the_start(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    await harness.mock.post_admin(
        "/_mock/stale-order", {"exchangeId": EX, "side": "ask", "through": 0.05, "quantity": 40}
    )
    await harness.write_fair()
    bridge = await harness.open_bridge("live")
    try:
        bridge.sniper.config = replace(bridge.sniper.config, unwind_from_utc=harness.now())
        assert await bridge.sniper.scan() == []
        assert await positions(harness) == {}
    finally:
        await bridge.close()


async def test_never_sells_a_locked_targets_leg(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("live")
    try:
        await hold_a_snipe(harness, bridge)
        now_ns = int(harness.now().timestamp() * 1e9)
        tf = build_target_file(
            tournament_slug="midterm-elections",
            seq=1,
            now_ns=now_ns,
            valid_for_s=36 * 3600,
            inputs=[],
            fair=[],
            positions=[
                targets_pb2.TargetPosition(exchange_id=EX, target_qty=40, max_entry_price=0.95, locked=True)
            ],
            cluster_limits={},
            tourney=targets_pb2.TourneyEstimate(),
            notes="test",
        )
        write_targets(harness.config.targets_dir, tf, pointer_seq=1)
        bridge.sniper.config = replace(bridge.sniper.config, unwind_from_utc=harness.now())
        bridge.sniper._last_action.clear()
        bridge.risk.cash = 1_000.0
        await harness.write_fair()
        outcomes = await bridge.sniper.scan()
        assert not [o for o in outcomes if o.request and o.request.exchange_id == EX]
        assert bridge.sniper.stats.blocked["unwind_locked"] >= 1
        # An unreadable pointer with no earlier good read: wait, never guess.
        (harness.config.targets_dir / "current.json").write_text("{not json")
        bridge.sniper._locked = None
        bridge.sniper._last_action.clear()
        assert await bridge.sniper.scan() == []
        assert bridge.sniper.stats.blocked["unwind_targets_unreadable"] >= 1
        assert (await positions(harness))[EX] == 40
    finally:
        await bridge.close()


async def test_stops_once_cash_reaches_the_target(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("live")
    try:
        await hold_a_snipe(harness, bridge)
        # The mock account is almost all cash: any target below that share is already met.
        bridge.sniper.config = replace(
            bridge.sniper.config, unwind_from_utc=harness.now(), unwind_target_cash_frac=0.5
        )
        bridge.sniper._last_action.clear()
        await harness.write_fair()
        assert await bridge.sniper.scan() == []
        assert bridge.sniper.stats.blocked["unwind_cash_target"] == 1
        (state,) = journal_events(bridge, "sniper_unwind_state")
        assert state["stop"] == "cash_target"
        await bridge.sniper.scan()
        assert len(journal_events(bridge, "sniper_unwind_state")) == 1, "a standing stop is journaled once"
        assert (await positions(harness))[EX] == 40
    finally:
        await bridge.close()
