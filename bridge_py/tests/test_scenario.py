"""EXP-008: the bridge's scenario executor works the live account toward the scenario book exactly as
rust-live does (``tests/fixtures/scenario_tranches.json``), through the gateway's gates and caps."""

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
from susq_bridge.config import ScenarioConfig, load_config
from susq_bridge.strategies.scenario import (
    NAME,
    BookTop,
    ScenarioPlanner,
    ScenarioView,
    Tranche,
    floor_from_prob,
    to_ns,
)
from susq_bridge.targets import TargetsReader

FIXTURE = Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "scenario_tranches.json"
SLUG = "midterm-elections"
EX = "1309"  # Michigan D (clean book at seed 7)


def scenario_config(raw: dict[str, Any]) -> ScenarioConfig:
    """A fixture ``config`` object (Rust ``ScenarioCfg`` JSON; missing keys take the defaults)."""
    kw: dict[str, Any] = dict(raw)
    if isinstance(kw.get("start_utc"), str):
        kw["start_utc"] = datetime.fromisoformat(kw["start_utc"].replace("Z", "+00:00"))
    return ScenarioConfig(**kw)


# ── config ──────────────────────────────────────────────────────────────────────────────────


def test_shipped_config_is_read_from_the_shared_section(home: Path) -> None:
    cfg = load_config(home, env={})
    assert cfg.scenario == ScenarioConfig(
        enabled=True,
        tranche_frac=0.25,
        min_tranche_qty=1,
        order_ttl_s=120.0,
        start_utc=datetime(2026, 10, 11, 16, 0, tzinfo=UTC),
        min_interval_s=900.0,
        max_hourly_notional_frac=0.02,
    )
    assert cfg.targets_max_age_s == 36 * 3600


def test_defaults_equal_rusts_scenario_cfg() -> None:
    # crates/core/src/config.rs `impl Default for ScenarioCfg`.
    assert ScenarioConfig() == scenario_config({})
    assert ScenarioConfig() == ScenarioConfig(True, 0.25, 1, 120.0, None, 0.0, 1.0)


@pytest.mark.parametrize(
    ("line", "bad"),
    [
        ("max_hourly_notional_frac = 0.02", "max_hourly_notional_frac = 1.5"),
        ('start_utc = "2026-10-11T16:00:00Z"', 'start_utc = "2026-10-11T16:00:00"'),
    ],
)
def test_unsafe_scenario_values_are_refused(home: Path, line: str, bad: str) -> None:
    toml = home / "config" / "susq.toml"
    text = toml.read_text()
    assert line in text
    toml.write_text(text.replace(line, bad, 1))
    with pytest.raises(ValueError, match=r"max_hourly_notional_frac|timezone"):
        load_config(home, env={})


# ── the rule (pure) ─────────────────────────────────────────────────────────────────────────


def test_floor_from_prob_matches_rust_pips() -> None:
    assert floor_from_prob(0.62) == 6200
    assert floor_from_prob(0.6249) == 6200
    assert floor_from_prob(0.0) is None
    assert floor_from_prob(0.004) is None
    assert floor_from_prob(0.999) == 9950
    assert floor_from_prob(1 - 0.72) == 2800
    assert floor_from_prob(float("nan")) is None


def test_tranche_sizes() -> None:
    p = ScenarioPlanner(ScenarioConfig())
    assert p.tranche(0, 100) == Tranche("yes", 25, False)
    assert p.tranche(0, -3) == Tranche("no", 1, False)
    assert p.tranche(100, 0) == Tranche("no", 25, True)
    assert p.tranche(10, -200) == Tranche("no", 10, True), "through zero: stop at flat first"
    assert p.tranche(7, 7) is None
    big_min = ScenarioPlanner(ScenarioConfig(min_tranche_qty=40))
    assert big_min.tranche(0, 100) == Tranche("yes", 40, False)
    assert big_min.tranche(0, 30) == Tranche("yes", 30, False), "never beyond the gap"


def fixture_book(m: dict[str, Any]) -> BookTop | None:
    if m["book"] is None or m["stale"]:
        return None
    return m["book"][0], m["book"][1]


def fixture_view(step: dict[str, Any]) -> ScenarioView:
    markets = step["markets"]
    rows = step["targets"]
    return ScenarioView(
        now_ns=step["now_s"] * 1_000_000_000,
        equity=step["equity"],
        targets=None
        if rows is None
        else [
            targets_pb2.TargetPosition(
                exchange_id=r["exchange"],
                target_qty=r["target_qty"],
                max_entry_price=r["max_entry_price"],
                locked=r["locked"],
            )
            for r in rows
        ],
        increase_allowed=step["increase_allowed"],
        verified={ex for ex, m in markets.items() if m["verified"]},
        positions={ex: m["position"] for ex, m in markets.items()},
        live={ex for ex, m in markets.items() if m["live_intent"]},
    )


async def test_parity_fixture_shared_with_rust() -> None:
    """``tests/fixtures/scenario_tranches.json`` is also run through rust-live's executor
    (``crates/strategies/src/scenario_executor.rs``): both traders must emit exactly these tranches."""
    fixture = json.loads(FIXTURE.read_text())
    assert fixture["tranches"]
    for t in fixture["tranches"]:
        got = ScenarioPlanner(scenario_config(t["config"])).tranche(t["position"], t["target"])
        want = None if t["expect"] is None else Tranche(**t["expect"])
        assert got == want, t
    assert fixture["cases"]
    for case in fixture["cases"]:
        planner = ScenarioPlanner(
            scenario_config(case["config"]), max_order_notional_frac=case["max_order_notional_frac"]
        )
        for i, step in enumerate(case["steps"]):
            markets = step["markets"]
            reads: list[str] = []

            async def book(
                ex: str, markets: dict[str, Any] = markets, reads: list[str] = reads
            ) -> BookTop | None:
                reads.append(ex)
                return fixture_book(markets[ex]) if ex in markets else None

            d = await planner.decide(fixture_view(step), book)
            got = [(o.exchange_id, o.side, o.quantity, o.price_pips) for o in d.orders]
            want = [(e["exchange"], e["side"], e["quantity"], e["price"]) for e in step["expect"]]
            assert got == want, f"{case['name']} step {i}: {d.notes}"
            for note in step["notes"]:
                assert any(note in n for n in d.notes), f"{case['name']} step {i}: {note!r} not in {d.notes}"
            # A book is only read for a tranche that passed every other guard (rate budget).
            assert len(reads) <= len(d.orders) + sum("no fresh book" in n for n in d.notes) + sum(
                "per-order cap" in n for n in d.notes
            )


async def test_the_clamp_keeps_every_tranche_under_the_risk_cap_until_the_target() -> None:
    """rust-live paper, Oct 11: a tranche over ``[risk] max_order_notional_frac`` was refused forever."""
    planner = ScenarioPlanner(ScenarioConfig(), max_order_notional_frac=0.02)
    row = targets_pb2.TargetPosition(exchange_id="1", target_qty=10_000, max_entry_price=0.9)
    pos = 0

    async def book(_: str) -> BookTop:
        return (3000, 3200)

    for now in range(50):
        view = ScenarioView(now, 27_260.0, [row], True, {"1"}, {"1": pos}, set())
        d = await planner.decide(view, book)
        if not d.orders:
            break
        (o,) = d.orders
        assert o.quantity * o.price_pips <= 0.02 * 27_260.0 * PIPS_PER_UNIT + 1e-6
        pos += o.quantity
    assert pos == 10_000


# ── targets reader ──────────────────────────────────────────────────────────────────────────


def write_book(
    targets_dir: Path,
    now: datetime,
    positions: list[targets_pb2.TargetPosition],
    *,
    seq: int = 1,
    slug: str = SLUG,
    valid_for_s: float = 36 * 3600,
) -> None:
    tf = build_target_file(
        tournament_slug=slug,
        seq=seq,
        now_ns=to_ns(now),
        valid_for_s=valid_for_s,
        inputs=[],
        fair=[],
        positions=positions,
        cluster_limits={},
        tourney=targets_pb2.TourneyEstimate(),
        notes="test",
    )
    write_targets(targets_dir, tf, pointer_seq=seq)


def leg(ex: str = EX, qty: int = 100, price: float = 0.9, locked: bool = False) -> targets_pb2.TargetPosition:
    return targets_pb2.TargetPosition(exchange_id=ex, target_qty=qty, max_entry_price=price, locked=locked)


def test_targets_reader_keeps_the_last_good_file_and_blocks_increases_when_it_is_not_current(
    tmp_path: Path,
) -> None:
    now = datetime(2026, 10, 11, 16, 0, tzinfo=UTC)
    r = TargetsReader(tmp_path, tournament_slug=SLUG, max_age_s=36 * 3600)
    r.refresh(to_ns(now))
    assert r.good is None
    assert not r.increase_allowed
    assert r.rejection == "no current.json"

    write_book(tmp_path, now, [leg(locked=True), leg("1310", -50)])
    r.refresh(to_ns(now))
    assert r.increase_allowed
    assert r.locked() == {EX}

    # Older than max_age_h: kept (locks and reductions stand), increases blocked.
    r.refresh(to_ns(now + timedelta(hours=37)))
    assert r.good is not None
    assert not r.increase_allowed
    assert r.stale is not None

    # A broken pointer: last good kept, increases blocked until a good file is current again.
    (tmp_path / "current.json").write_text("{not json")
    r.refresh(to_ns(now))
    assert r.good is not None
    assert r.rejection is not None
    assert not r.increase_allowed
    write_book(tmp_path, now, [leg()], seq=2)
    r.refresh(to_ns(now))
    assert r.increase_allowed
    assert r.locked() == set()

    # Another tournament's file is refused.
    write_book(tmp_path, now, [leg(qty=999)], seq=3, slug="other")
    r.refresh(to_ns(now))
    assert not r.increase_allowed
    assert r.good is not None
    assert r.good.positions[0].target_qty == 100

    # A file produced too far in the future is refused (clock skew beyond 5 minutes).
    write_book(tmp_path, now + timedelta(minutes=6), [leg(qty=7)], seq=4)
    r.refresh(to_ns(now))
    assert not r.increase_allowed
    r.refresh(to_ns(now + timedelta(minutes=6)))
    assert r.increase_allowed
    assert r.good is not None
    assert r.good.positions[0].target_qty == 7


# ── the bridge, end to end on the mock ──────────────────────────────────────────────────────


def journal_events(bridge, kind: str) -> list[dict[str, Any]]:  # type: ignore[no-untyped-def]
    bridge.journal.flush()
    out = []
    for path in sorted(bridge.config.journal_dir.rglob("bridge.journal.jsonl")):
        for line in path.read_text().splitlines():
            entry = json.loads(line) if line.strip() else {}
            if entry.get("method") == kind:
                out.append(json.loads(entry.get("bodyJson") or entry["body_json"]))
    return out


async def open_scenario(harness, positions: list[targets_pb2.TargetPosition], **cfg: Any):  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    write_book(harness.config.targets_dir, harness.now(), positions)
    bridge = await harness.open_bridge("live")
    bridge.scenario.config = replace(bridge.scenario.config, start_utc=None, **cfg)
    return bridge


async def test_buys_a_tranche_at_the_ask_through_the_gateway(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await open_scenario(harness, [leg(qty=100, price=0.95)])
    try:
        _, ask = await harness.top(EX)
        assert ask <= 9500
        (out,) = await bridge.scenario.scan()
        assert out.placed, out.reason
        assert out.intent_id.startswith(f"{NAME}:{EX}:")
        req = out.request
        want = (EX, "yes", "buy", 25, ask)
        assert (req.exchange_id, req.side, req.action, req.quantity, req.price_pips) == want
        (event,) = journal_events(bridge, "scenario_order")
        assert (event["status"], event["quantity"], event["target"]) == ("placed", 25, 100)
        # The tranche filled at the ask: the next pass waits out min_interval_s on this exchange.
        await bridge.oms.reconcile()
        assert bridge.risk.positions[EX] == 25
        bridge.scenario.config = replace(bridge.scenario.config, min_interval_s=900)
        assert await bridge.scenario.scan() == []
        assert any("waiting" in n for n in journal_events(bridge, "scenario_pass")[-1]["notes"])
    finally:
        await bridge.close()


async def test_rests_at_the_cap_and_does_not_stack_on_a_working_order(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await open_scenario(harness, [leg(qty=100, price=0.05)])
    try:
        bid, _ = await harness.top(EX)
        assert bid > 500, "the cap sits below the book, so the tranche rests"
        (out,) = await bridge.scenario.scan()
        assert out.placed
        assert out.request.price_pips == 500
        assert [o.exchange_id for o in bridge.oms.open_orders(strategy=NAME)] == [EX]
        assert await bridge.scenario.scan() == [], "a resting scenario order blocks the next tranche"
    finally:
        await bridge.close()


async def test_clamps_an_oversized_tranche_to_the_per_order_cap(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await open_scenario(harness, [leg(qty=1_000_000, price=0.95)])
    try:
        (out,) = await bridge.scenario.scan()
        assert out.placed, out.reason
        req = out.request
        cap = bridge.config.risk.max_order_notional_frac * bridge.risk.equity
        assert req.quantity * req.price_pips <= cap * PIPS_PER_UNIT + 1e-6
        assert req.quantity < 250_000, "a quarter of the gap would have been refused by risk"
    finally:
        await bridge.close()


async def test_stale_targets_send_nothing_new_and_locked_legs_reach_risk(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await open_scenario(harness, [leg(qty=100, locked=True)])
    try:
        bridge.scenario.targets.refresh(to_ns(harness.now()))  # the first file is adopted
        # The latest file is 37 h old (max_age_h is 36): it is refused and the last good file is kept.
        write_book(
            harness.config.targets_dir,
            harness.now() - timedelta(hours=37),
            [leg(qty=100, locked=True)],
            seq=2,
            valid_for_s=48 * 3600,
        )
        assert await bridge.scenario.scan() == []
        assert not bridge.scenario.targets.increase_allowed
        (state,) = journal_events(bridge, "scenario_pass")
        assert any("not increasing exposure" in n for n in state["notes"])
        assert bridge.risk.locked == {EX}
    finally:
        await bridge.close()


async def test_nothing_before_start_utc_or_while_gated(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await open_scenario(harness, [leg()])
    try:
        bridge.scenario.config = replace(bridge.scenario.config, start_utc=harness.now() + timedelta(hours=1))
        assert await bridge.scenario.scan() == []
        assert any("not started" in n for n in journal_events(bridge, "scenario_pass")[-1]["notes"])
        bridge.scenario.config = replace(bridge.scenario.config, start_utc=None)
        bridge.risk.trip("test")
        assert await bridge.scenario.scan() == []
        assert bridge.scenario.stats.blocked.get("frozen") == 1
    finally:
        await bridge.close()


async def test_unverified_markets_are_never_traded(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map(verified=False)
    write_book(harness.config.targets_dir, harness.now(), [leg()])
    bridge = await harness.open_bridge("live")
    try:
        bridge.scenario.config = replace(bridge.scenario.config, start_utc=None)
        assert await bridge.scenario.scan() == []
    finally:
        await bridge.close()
