"""susq_models.sim against the numpy oracle and its own invariants."""

from __future__ import annotations

import numpy as np
import pytest
from reference_sim import RefRace, RefShocks, analytic_covariance, simulate
from susq_models import sim

CLUSTERS = {"red-state-reach": 2.0, "midwest-industrial": 1.5}

REF_RACES = [
    RefRace("OH", "Midwest", mu=1.0, loadings={"red-state-reach": 1.0, "midwest-industrial": 1.0}),
    RefRace("OH", "Midwest", mu=-2.0, sd=3.5),
    RefRace("MI", "Midwest", mu=3.0, loadings={"midwest-industrial": 1.0}),
    RefRace("TX", "South", mu=0.5, beta=0.8, loadings={"red-state-reach": 1.0}),
    RefRace("NE", "Midwest", mu=-1.5, p_third_party=0.25),
    RefRace("ME", "Northeast", mu=2.0, sd=2.5),
]


def build_model(n_draws: int, seed: int = 11, threads: int = 0) -> sim.Model:
    cfg = sim.default_config()
    cfg.n_draws = n_draws
    cfg.seed = seed
    cfg.threads = threads
    cfg.clusters = [sim.ClusterSpec(name=k, sd=v) for k, v in CLUSTERS.items()]
    cfg.races = [
        sim.RaceSpec(
            id=f"race-{i}",
            state=r.state,
            mu=r.mu,
            sd=r.sd,
            national_beta=r.beta,
            p_third_party=r.p_third_party,
            clusters=[sim.ClusterLoading(cluster=k, loading=v) for k, v in r.loadings.items()],
        )
        for i, r in enumerate(REF_RACES)
    ]
    return sim.Model(cfg)


def test_marginals_match_numpy_reference() -> None:
    n = 300_000
    model = build_model(n)
    model.generate()
    model.evaluate()
    ref = simulate(REF_RACES, RefShocks(), CLUSTERS, n, seed=5)
    for i in range(len(REF_RACES)):
        for code in (0, 1, 2):
            p_cpp = float(np.mean(model.outcomes[i] == code))
            p_ref = float(np.mean(ref[i] == code))
            se = np.sqrt(2 * max(p_ref * (1 - p_ref), 1e-6) / n)
            assert abs(p_cpp - p_ref) < 5 * se + 1e-4, (i, code, p_cpp, p_ref)


def test_error_covariance_matches_closed_form() -> None:
    model = build_model(400_000)
    model.generate()
    emp = np.cov(model.race_errors.astype(np.float64))
    exact = analytic_covariance(REF_RACES, RefShocks(), CLUSTERS)
    np.testing.assert_allclose(emp, exact, atol=0.35, rtol=0.03)
    corr = np.corrcoef(model.race_errors)
    assert corr[0, 1] > corr[0, 2] > corr[0, 5]  # same state > same region > cross region


def test_calibration_hits_market_targets() -> None:
    cfg = build_model(100_000).config
    targets = [0.58, 0.43, 0.72, 0.61, 0.30, 0.60]
    races = cfg.races
    for r, t in zip(races, targets, strict=True):
        r.target_p_win = t
    cfg.races = races
    model = sim.Model(cfg)
    model.run()
    np.testing.assert_allclose(model.p_dem_side, targets, atol=1e-3)
    report = model.calibrate()
    assert all(e["feasible"] for e in report)
    np.testing.assert_allclose(model.mu, [e["mu"] for e in report])


def test_scenario_bundles_are_a_partition() -> None:
    model = build_model(50_000)
    model.generate()
    model.evaluate()
    b = model.scenario_bundles(["red-state-reach"])
    assert b["p_dem_market"].shape == (20, len(REF_RACES))
    assert b["probability"].sum() == pytest.approx(1.0)
    mask = b["n_draws"] > 0
    total = (b["probability"][mask, None] * b["p_dem_market"][mask]).sum(axis=0)
    np.testing.assert_allclose(total, model.p_dem_market, atol=1e-9)
    assert np.bincount(b["draw_bucket"], minlength=20).tolist() == b["n_draws"].tolist()


def test_results_do_not_depend_on_thread_count() -> None:
    one = build_model(30_000, threads=1)
    many = build_model(30_000, threads=6)
    one.generate()
    many.generate()
    np.testing.assert_array_equal(one.race_errors, many.race_errors)
    np.testing.assert_array_equal(one.national, many.national)


def test_house_and_senate_aggregation() -> None:
    cfg = sim.default_config()
    cfg.n_draws = 5_000
    cfg.senate = sim.SenateSpec(dem_seats_not_up=50, vp_breaks_ties_dem=True)
    cfg.races = [sim.RaceSpec(id="safe-r", state="SC", chamber=sim.Chamber.SENATE, mu=-100.0)]
    cfg.house = sim.HouseSpec(generic_ballot=0.0, control_threshold=2)
    cfg.districts = [
        sim.HouseDistrictSpec(id="A", state="CA", lean=100.0),
        sim.HouseDistrictSpec(id="B", state="NY", lean=100.0),
        sim.HouseDistrictSpec(id="C", state="TX", lean=-100.0),
    ]
    model = sim.Model(cfg)
    model.run()
    assert model.p_senate_dem_control == 1.0  # 50 + VP
    assert set(model.house_dem_seats.tolist()) == {2}
    assert model.p_house_dem_control == 1.0


def test_errors_are_python_exceptions() -> None:
    model = build_model(1_000)
    with pytest.raises(RuntimeError, match="first"):
        _ = model.outcomes
    cfg = model.config
    cfg.shocks.national_nu = 1.5
    with pytest.raises(ValueError, match="national_nu"):
        sim.Model(cfg)
    with pytest.raises(AttributeError):
        sim.RaceSpec(nonexistent=1)
