"""Drill harness: executor simulation, false triggers, health gating and the report."""

from __future__ import annotations

import pytest
from susq.contracts import bus_pb2
from susq.replay.drill import ExecutorConfig, render_report, simulate_executor, write_report
from susq.replay.needle_ref import ReferenceNeedle
from susq.replay.records import RaceTruth
from susq.replay.runner import (
    ChaosConfig,
    HealthEvent,
    Kind,
    PosteriorSample,
    ReplayRecord,
    replay_in_process,
)
from susq.replay.spec import load_spec, materialize

from .conftest import SAMPLE_SPEC


@pytest.fixture(scope="module")
def sample():
    spec = load_spec(SAMPLE_SPEC)
    return materialize(spec)


def _run(sample, chaos: ChaosConfig | None = None, **cfg: float):
    batches, meta, prior = sample
    needle = ReferenceNeedle()
    needle.load_prior(prior)
    rec = replay_in_process(batches, needle, chaos=chaos or ChaosConfig())
    config = ExecutorConfig(**cfg)  # type: ignore[arg-type]
    return simulate_executor(rec, prior, meta.races, config), config


def test_sample_spec_builds_three_races(sample) -> None:
    batches, meta, prior = sample
    assert set(meta.races) == {"2022-SEN-GA", "2022-SEN-NH", "2022-GOV-ME"}
    assert len(prior.races) == 3
    assert batches


def test_latency_edge_makes_money_against_a_lagging_book(sample) -> None:
    result, _ = _run(sample, market_delay_s=900.0)
    assert result.trades, "a book 15 minutes behind must leave edge"
    assert result.pnl > 0
    assert all(t.decision_latency_s >= 0 for t in result.trades)
    assert len({(t.race_id, t.side) for t in result.trades}) == len(result.trades), (
        "one position per race-side"
    )


def test_no_lag_means_no_edge(sample) -> None:
    result, _ = _run(sample, market_delay_s=0.0)
    assert result.trades == []


def test_chaos_drill_report(sample, tmp_path) -> None:
    chaos = ChaosConfig(
        drop_start_s=1800, drop_duration_s=900, malformed_every=7, kill_at_s=5400, restart_after_s=600
    )
    result, cfg = _run(sample, chaos=chaos)
    rec = result.record
    assert rec.malformed_rejected == rec.malformed_injected > 0
    assert rec.kills == rec.restarts == 1
    assert result.unhealthy_s > 0
    text = render_report(result, title="t", replay="spec", backend_note="python-reference", cfg=cfg)
    for needle in (
        "Latency arrival → posterior",
        "False",
        "P&L vs truth",
        "Malformed injected / rejected",
        "Chaos:",
    ):
        assert needle in text
    p1 = write_report(text, tmp_path, date="2026-10-12")
    p2 = write_report(text, tmp_path, date="2026-10-12")
    assert p1.name == "2026-10-12.md"
    assert p2.name == "2026-10-12-2.md"


def _post(race: str, p: float, n: int = 5) -> bus_pb2.RacePosterior:
    return bus_pb2.RacePosterior(race_id=race, p_dem=p, p_rep=1 - p, margin_sd=1.0, n_counties_reporting=n)


def _record(ps: list[float], health: list[HealthEvent] | None = None) -> ReplayRecord:
    rec = ReplayRecord(backend="x", speed=0, chaos=ChaosConfig())
    for i, p in enumerate(ps):
        t = 60.0 * (i + 1)
        rec.samples.append(PosteriorSample(t, 0.001, i + 1, {"R": _post("R", p)}))
        rec.health.append(HealthEvent(t, True, Kind.BATCH))
    if health is not None:
        rec.health = health
    return rec


def _prior():
    from susq.contracts import needle_pb2

    p = needle_pb2.NeedlePrior()
    p.races.add(race_id="R", state="GA", prior_margin_mean=0.0, prior_margin_sd=5.0)
    return p


def _truth(dem_wins: bool) -> dict[str, RaceTruth]:
    d, r = (60, 40) if dem_wins else (40, 60)
    return {"R": RaceTruth("R", "GA", d, r, 0, d, r, 0.0)}


def test_false_trigger_on_reversal_and_on_losing_side() -> None:
    rec = _record([0.5, 0.9, 0.95, 0.4, 0.3, 0.2])
    result = simulate_executor(rec, _prior(), _truth(dem_wins=False), ExecutorConfig(market_delay_s=0.0))
    dem = [t for t in result.triggers if t.side == "DEM"]
    assert len(dem) == 1
    assert dem[0].false
    assert "reversed" in dem[0].reason
    rec2 = _record([0.5, 0.9, 0.95, 0.96])
    result2 = simulate_executor(rec2, _prior(), _truth(dem_wins=False), ExecutorConfig(market_delay_s=0.0))
    assert result2.triggers[0].false
    assert "lost" in result2.triggers[0].reason
    result3 = simulate_executor(rec2, _prior(), _truth(dem_wins=True), ExecutorConfig(market_delay_s=0.0))
    assert result3.false_triggers == 0


def test_orders_are_suppressed_while_input_is_stale() -> None:
    # Posterior jumps to 0.99 right after a 10-minute silence: the gate must hold the order until
    # three healthy inputs have arrived.
    ps = [0.5, 0.5, 0.99, 0.99, 0.99, 0.99]
    times = [60, 120, 720, 780, 840, 900]
    rec = ReplayRecord(backend="x", speed=0, chaos=ChaosConfig())
    for i, (t, p) in enumerate(zip(times, ps, strict=True)):
        rec.samples.append(PosteriorSample(float(t), 0.001, i + 1, {"R": _post("R", p)}))
        rec.health.append(HealthEvent(float(t), True, Kind.BATCH))
    cfg = ExecutorConfig(market_delay_s=10_000.0, stale_after_s=90.0, resume_after_healthy=3, se_mult=0.0)
    result = simulate_executor(rec, _prior(), _truth(dem_wins=True), cfg)
    assert result.suppressed_orders == 2  # t=720 and t=780
    assert [t.t_s for t in result.trades] == [840.0]
    assert result.unhealthy_s == pytest.approx(840 - (120 + 90))
