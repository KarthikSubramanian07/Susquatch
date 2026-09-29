"""Scenario grid, scenario scoring with crowding, and book construction invariants."""

from __future__ import annotations

import numpy as np
import pytest
from susq.mapping.geo import clusters_for, region_of
from susq.models.calibration import apply_mu, calibrate
from susq.models.chamber import senate_dem_control, senate_seat_specs
from susq.models.numpy_engine import NumpySimEngine
from susq.models.types import RaceSpec, SimParams
from susq.optimizer.book import TICK, build_book, score_scenarios
from susq.optimizer.legs import Leg, leg_outcomes
from susq.optimizer.scenarios import Scenario, scenario_grid
from susq.research_env import OptimizerConfig

CFG = OptimizerConfig()
EXTRA = {
    "2026-GOV-GA": ("GA", ()),
    "2026-GOV-OH": ("OH", ("red-state-reach", "midwest-industrial")),
    "2026-HOUSE-NY-17": ("NY", ("house",)),
}


@pytest.fixture(scope="module")
def world():
    specs = senate_seat_specs([])
    specs += [RaceSpec(rid, st, region_of(st), cl) for rid, (st, cl) in EXTRA.items()]
    specs = [
        RaceSpec(s.race_id, s.state, s.region, s.clusters, left_party="IND") if s.state == "NE" else s
        for s in specs
    ]
    shocks = NumpySimEngine().simulate(specs, SimParams(), 60_000, seed=5)
    targets = {s.race_id: 0.5 for s in specs}
    targets.update({"2026-SEN-TX": 0.6, "2026-SEN-OH-S": 0.58, "2026-SEN-IA": 0.42, "2026-SEN-AK": 0.7})
    targets.update({"2026-SEN-KS": 0.3, "2026-SEN-SC": 0.13, "2026-SEN-GA": 0.95, "2026-SEN-ME": 0.6})
    draws = apply_mu(shocks, calibrate(shocks, targets))
    return specs, draws


def _leg(rid: str, party: str, ask: float, kind: str = "race_party") -> Leg:
    state = rid.split("-")[2] if kind != "chamber" else ""
    clusters = tuple(clusters_for(state, house="HOUSE" in rid)) if state else ("national",)
    return Leg(f"x-{rid}-{party}", f"m-{rid}", rid, rid, party, ask, None, ask, min(0.995, ask + 0.03),
               clusters, state, kind, True)  # fmt: skip


@pytest.fixture(scope="module")
def legs():
    return [
        _leg("2026-SEN-TX", "REP", 0.40), _leg("2026-SEN-TX", "DEM", 0.61),
        _leg("2026-SEN-OH-S", "REP", 0.43), _leg("2026-SEN-IA", "REP", 0.58),
        _leg("2026-SEN-AK", "REP", 0.31), _leg("2026-SEN-KS", "DEM", 0.30),
        _leg("2026-SEN-SC", "DEM", 0.13), _leg("2026-SEN-ME", "REP", 0.40),
        _leg("2026-SEN-GA", "REP", 0.05), _leg("2026-GOV-GA", "DEM", 0.50),
        _leg("2026-GOV-OH", "REP", 0.50), _leg("2026-HOUSE-NY-17", "DEM", 0.5),
        _leg("2026-SEN-NE", "IND", 0.30), _leg("2026-SEN-NE", "DEM", 0.01),
        _leg("2026-CONTROL-SENATE", "REP", 0.39, kind="chamber"),
    ]  # fmt: skip


def test_leg_outcomes_follow_parties_and_chambers(world, legs) -> None:
    specs, draws = world
    y, kept = leg_outcomes(draws, specs, legs)
    assert len(kept) == len(legs)
    assert y.shape == (draws.n, len(legs))
    col = {(leg.race_id, leg.party): i for i, leg in enumerate(kept)}
    tx_r, tx_d = y[:, col[("2026-SEN-TX", "REP")]], y[:, col[("2026-SEN-TX", "DEM")]]
    assert np.all(tx_r ^ tx_d)
    assert not y[:, col[("2026-SEN-NE", "DEM")]].any()  # independent race: the Democrat never wins in-sim
    assert np.array_equal(y[:, col[("2026-CONTROL-SENATE", "REP")]], ~senate_dem_control(draws, specs))
    assert tx_r.mean() == pytest.approx(0.4, abs=0.01)


def test_unsimulated_legs_are_dropped(world) -> None:
    specs, draws = world
    y, kept = leg_outcomes(draws, specs, [_leg("2026-GOV-WY", "DEM", 0.5)])
    assert kept == []
    assert y.shape == (draws.n, 0)


def test_scenario_grid_partitions_the_worlds(world) -> None:
    _, draws = world
    grid = scenario_grid(draws, min_worlds=1)
    total = np.zeros(draws.n, dtype=int)
    for s in grid:
        total += s.mask
    assert np.all(total == 1)  # every world in exactly one scenario
    assert len(grid) <= 10 * 2 ** len(draws.clusters)
    low = [s for s in grid if s.decile == 0]
    assert all(s.signs["national"] == -1 for s in low)
    assert "R-leaning" in low[0].describe()
    assert sum(s.prob for s in grid) == pytest.approx(1.0)


def test_book_invariants(world, legs) -> None:
    specs, draws = world
    y, kept = leg_outcomes(draws, specs, legs)
    threshold = np.full(draws.n, 1.4)
    res = build_book(kept, y, scenario_grid(draws), threshold, CFG)
    assert res.best is not None
    assert res.positions
    s = res.best.scenario
    cond = y[s.mask].mean(axis=0)
    by_id = {leg.exchange_id: i for i, leg in enumerate(kept)}
    core = [p for p in res.positions if p.tranche != "T3"]
    idio = [p for p in res.positions if p.tranche == "T3"]
    assert len(idio) == 1
    assert not set(p.exchange_id for p in core) & {idio[0].exchange_id}
    assert all(cond[by_id[p.exchange_id]] >= CFG.min_leg_prob_in_scenario for p in core)
    assert all(p.target_qty > 0 and (p.target_qty <= 5 or p.target_qty % 5) for p in res.positions)
    assert all(abs(p.max_entry_price / TICK - round(p.max_entry_price / TICK)) < 1e-9 for p in res.positions)
    assert all(p.max_entry_price >= kept[by_id[p.exchange_id]].ask - 1e-12 for p in res.positions)
    assert res.book_cost <= (CFG.book_frac + CFG.idiosyncratic_frac) * 100_000 + 1
    assert all(p.rationale and p.cluster for p in res.positions)
    assert {p.tranche for p in core} <= {"T1", "T2"}
    assert 0.0 <= res.p_first <= 1.0
    assert res.p_top3 == pytest.approx(min(1.0, 2.5 * res.p_first))
    assert s.scenario_id in res.explanation


def test_locked_flags_follow_reporting_time(world, legs) -> None:
    specs, draws = world
    y, kept = leg_outcomes(draws, specs, legs)
    res = build_book(kept, y, scenario_grid(draws), np.full(draws.n, 1.2), CFG)
    for p in res.positions:
        if p.tranche == "T3":
            assert not p.locked
            continue
        state = p.exchange_id.split("-")[3] if "CONTROL" not in p.exchange_id else ""
        early = state in {"GA", "SC", "OH", "TX", "KS", "ME"}
        assert p.locked == (not early), p.exchange_id
        assert ("rolling fuel" in p.rationale) == early


def test_crowding_penalizes_scenarios_the_field_is_loaded_on(world, legs) -> None:
    specs, draws = world
    y, kept = leg_outcomes(draws, specs, legs)
    grid = scenario_grid(draws)
    threshold = np.full(draws.n, 1.3)
    neutral = {s.scenario.scenario_id: s for s in score_scenarios(kept, y, grid, threshold, CFG)}
    long_d = {
        s.scenario.scenario_id: s
        for s in score_scenarios(kept, y, grid, threshold, CFG, cluster_drift_pts={"national": 5.0})
    }
    for sid, n in neutral.items():
        crowded = long_d[sid]
        if n.scenario.signs["national"] > 0:
            assert crowded.exposure > 0
            assert crowded.score <= n.score + 1e-12
        else:
            assert crowded.exposure < 0
            assert crowded.score >= n.score - 1e-12


def test_concentrating_in_cheap_legs_beats_spreading() -> None:
    """Two cheap legs that both pay in S reach T; adding the pricey leg would dilute them below it."""
    rng = np.random.default_rng(0)
    n = 20_000
    in_s = rng.random(n) < 0.2
    y = np.column_stack([in_s, in_s, rng.random(n) < 0.9])  # cheap, cheap, expensive-and-likely
    legs = [
        Leg("a", "a", "a", "r", "DEM", 0.04, None, 0.04, 0.05, ("national",), "TX", "race_party", True),
        Leg("b", "b", "b", "r", "DEM", 0.05, None, 0.05, 0.06, ("national",), "TX", "race_party", True),
        Leg("c", "c", "c", "r", "DEM", 0.85, None, 0.85, 0.90, ("national",), "TX", "race_party", True),
    ]
    s = Scenario("S", 0, {"national": -1}, in_s)
    cfg = OptimizerConfig(max_leg_frac=0.35, min_leg_prob_in_scenario=0.3, min_lift=1.0)
    # Hitting: one leg -> 9.4x, both cheap legs -> 13.9x, all three -> ~9.6x. Only the pair clears 12x.
    (score,) = score_scenarios(legs, y, [s], np.full(n, 12.0), cfg)
    assert set(score.legs) == {0, 1}  # the 85c leg is eligible but left out
    assert score.p_beat_given_s == 1.0
    assert score.score == pytest.approx(s.prob)


def test_book_is_deterministic_and_handles_empty_input(world, legs) -> None:
    specs, draws = world
    y, kept = leg_outcomes(draws, specs, legs)
    grid, t = scenario_grid(draws), np.full(draws.n, 1.4)
    a, b = build_book(kept, y, grid, t, CFG), build_book(kept, y, grid, t, CFG)
    assert a.positions == b.positions
    empty = build_book([], np.zeros((draws.n, 0), bool), grid, t, CFG)
    assert empty.positions == []
    assert "no scenario" in empty.explanation
