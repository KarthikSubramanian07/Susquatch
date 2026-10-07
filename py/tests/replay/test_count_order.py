"""EXP-006 count-order table: the Python reader agrees with the Rust one, and the drill gate."""

from __future__ import annotations

import pytest
from susq.replay.count_order import CountOrderTable, Profile, Registered, band_prob
from susq.replay.drill import (
    ExecutorConfig,
    compare_count_order,
    render_comparison,
    simulate_executor,
)
from susq.replay.needle_ref import ReferenceNeedle
from susq.replay.runner import ChaosConfig, replay_in_process
from susq.replay.spec import load_spec, materialize

from .conftest import SAMPLE_SPEC

STATES = [
    *("AK", "AZ", "CA", "GA", "IA", "KS", "ME", "MI", "NC"),
    *("NE", "NH", "NV", "OH", "PA", "SC", "TX", "WA", "WI"),
]


@pytest.fixture(scope="module")
def table() -> CountOrderTable:
    return CountOrderTable.load()


def _profile(table: CountOrderTable, race: str, state: str | None = None) -> Profile:
    p = table.profile(race, state)
    assert p is not None, race
    return p


def _at(p: Profile, h: float) -> Registered:
    r = p.at_hour(h)
    assert r is not None
    return r


def test_shipped_table_has_the_18_states(table: CountOrderTable) -> None:
    assert table.states == STATES


def test_interpolation_matches_the_rust_reader(table: CountOrderTable) -> None:
    nv = _profile(table, "2026-GOV-NV")
    assert nv.at_hour(-0.1) is None
    assert _at(nv, 0.0).drift_pts == 2.5
    assert _at(nv, 2.0).drift_pts == pytest.approx(2.5 + 0.5 * (-2.25 - 2.5))
    assert _at(nv, 14.0).drift_pts == -2.0
    mid = _at(nv, 91.0)
    assert mid.drift_pts == pytest.approx(-1.0)
    assert mid.band_pts == pytest.approx(0.375)
    assert _at(nv, 500.0).drift_pts == 0.0
    assert nv.lock().favours() == "DEM"


def test_overrides_and_directions(table: CountOrderTable) -> None:
    assert _profile(table, "2026-HOUSE-CA-13").key == "2026-HOUSE-CA-*"
    assert _profile(table, "2026-GOV-CA").key == "CA"
    wa3 = _profile(table, "2026-HOUSE-WA-03").lock()
    assert wa3.band_pts == 5.0
    assert wa3.favours() is None
    assert _profile(table, "2026-HOUSE-ME-02").lock().favours() == "REP"
    assert table.profile("2026-SEN-FL-S") is None
    assert table.profile("2026-CONTROL-SENATE") is None
    # A 2022 race id resolves through its state row.
    assert _profile(table, "2022-SEN-GA", "GA").key == "GA"


def test_band_prob_matches_the_executor() -> None:
    assert band_prob(2.0, 2.0, 1.0, "DEM") == pytest.approx(0.8413447 - 0.6914625, abs=1e-6)
    assert band_prob(2.0, 2.0, 1.0, "REP") == pytest.approx(0.9331928 - 0.8413447, abs=1e-6)
    assert band_prob(2.0, 0.0, 1.0, "DEM") is None
    assert band_prob(float("nan"), 2.0, 1.0, "DEM") is None


def test_drill_compare_runs_the_same_record_with_the_gate_off_and_on(table: CountOrderTable) -> None:
    batches, meta, prior = materialize(load_spec(SAMPLE_SPEC))
    needle = ReferenceNeedle()
    needle.load_prior(prior)
    rec = replay_in_process(batches, needle, chaos=ChaosConfig())
    cfg = ExecutorConfig(market_delay_s=900.0)
    cmp = compare_count_order(rec, prior, meta.races, cfg, table)
    assert cmp.off.gate == "off"
    assert cmp.on.gate == "on"
    assert not cmp.off.gate_blocked
    # The gate only ever adds to the bar: every gate-on trade is also a gate-off trade.
    off = {(t.race_id, t.side, t.t_s) for t in cmp.off.trades}
    assert {(t.race_id, t.side, t.t_s) for t in cmp.on.trades} <= off
    # Gate-off equals the plain simulator.
    plain = simulate_executor(rec, prior, meta.races, cfg)
    assert [(t.race_id, t.side) for t in plain.trades] == [(t.race_id, t.side) for t in cmp.off.trades]
    text = render_comparison(cmp, title="sample", replay="x", backend_note="reference", cfg=cfg, table=table)
    assert "| Gate off |" in text
    assert "| Gate on |" in text
    assert "not a test of the hypothesis" in text
    assert "no 2024 fixture" in text
