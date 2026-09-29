"""Numpy reference simulator, calibration, chamber aggregation and tourney: statistical properties."""

from __future__ import annotations

import sys
import types

import numpy as np
import pytest
from hypothesis import given, settings
from hypothesis import strategies as st
from susq.mapping.geo import SENATE_SEATS_UP, region_of
from susq.mapping.races import IND, REP, senate
from susq.models import facade
from susq.models.calibration import apply_mu, bisect_mu, calibrate
from susq.models.chamber import (
    HOUSE_CONTROL_ID,
    house_control_spec,
    house_dem_control,
    senate_dem_control,
    senate_dem_seats,
    senate_seat_specs,
)
from susq.models.numpy_engine import NumpySimEngine, analytic_margin_sd
from susq.models.tourney import FieldParams, p_first, prior_threshold, simulate_threshold
from susq.models.types import RaceSpec, SimParams

N = 120_000


def _race(rid: str, state: str, clusters: tuple[str, ...] = (), mu: float = 0.0) -> RaceSpec:
    return RaceSpec(rid, state, region_of(state), clusters, mu=mu)


@pytest.fixture(scope="module")
def draws():
    races = [
        _race("2026-SEN-OH-S", "OH", ("midwest-industrial", "red-state-reach")),
        _race("2026-GOV-OH", "OH", ("midwest-industrial", "red-state-reach")),
        _race("2026-SEN-MI", "MI", ("midwest-industrial",), mu=4.0),
        _race("2026-SEN-IA", "IA", ("red-state-reach",)),
        _race("2026-SEN-GA", "GA", mu=-2.0),
        _race("2026-SEN-ME", "ME", ("new-england-incumbency",)),
    ]
    return races, NumpySimEngine().simulate(races, SimParams(), N, seed=7)


def test_margin_moments_match_parameters(draws) -> None:
    races, d = draws
    params = SimParams()
    for i, r in enumerate(races):
        col = d.margins[:, i]
        assert col.mean() == pytest.approx(r.mu, abs=0.06)
        assert col.std() == pytest.approx(analytic_margin_sd(r, params), rel=0.02)
    # Total SD lands in SPEC §5's 4.5-5.5 point band for a plain state race.
    assert 4.5 <= analytic_margin_sd(RaceSpec("x", "GA", "south"), params) <= 5.5


def test_national_shift_is_fat_tailed_with_miss_mixture(draws) -> None:
    _, d = draws
    params = SimParams()
    expected_var = params.national_sd**2 + params.miss_p * params.miss_shift**2
    assert d.national.var() == pytest.approx(expected_var, rel=0.04)
    kurt = ((d.national - d.national.mean()) ** 4).mean() / d.national.var() ** 2
    assert kurt > 3.5  # t(5) plus a mixture is well above Gaussian kurtosis


def test_correlation_ordering_same_state_gt_cluster_gt_region_gt_national(draws) -> None:
    _, d = draws
    c = np.corrcoef(d.margins.T)
    same_state = c[0, 1]  # OH Sen vs OH Gov
    shared_cluster = c[0, 2]  # OH vs MI: same region + midwest-industrial
    same_region = c[2, 3]  # MI vs IA: same region, no shared named cluster
    national_only = c[4, 5]  # GA vs ME
    assert same_state > shared_cluster > same_region > national_only > 0.2


def test_seeded_runs_are_reproducible() -> None:
    races = [_race("2026-SEN-TX", "TX", ("red-state-reach",))]
    a = NumpySimEngine(chunk=1000).simulate(races, SimParams(), 5000, seed=3)
    b = NumpySimEngine(chunk=1000).simulate(races, SimParams(), 5000, seed=3)
    np.testing.assert_array_equal(a.margins, b.margins)


def test_invalid_params_rejected() -> None:
    with pytest.raises(ValueError, match="nu"):
        SimParams(nu=2.0)
    with pytest.raises(ValueError, match="n_draws"):
        NumpySimEngine().simulate([_race("r", "TX")], SimParams(), 0, seed=0)


@settings(max_examples=40, deadline=None)
@given(st.floats(min_value=0.01, max_value=0.99))
def test_bisection_hits_target_probability(p: float) -> None:
    shock = np.random.default_rng(0).normal(0, 5, 20_000)
    mu = bisect_mu(shock, p)
    assert np.mean(shock + mu > 0) == pytest.approx(p, abs=2e-4 + 1 / 20_000)


def test_calibration_reproduces_market_marginals(draws) -> None:
    races, d = draws
    shocks = d.with_mu([-r.mu for r in races])
    targets = {"2026-SEN-OH-S": 0.58, "2026-SEN-MI": 0.72, "2026-SEN-GA": 0.955, "2026-SEN-ME": 0.60}
    mu = calibrate(shocks, targets)
    fitted = apply_mu(shocks, mu)
    for rid, p in targets.items():
        assert fitted.left_wins()[:, fitted.column(rid)].mean() == pytest.approx(p, abs=0.002)
    # Higher target probability => larger μ (monotone), and extremes are clipped, not infinite.
    assert mu["2026-SEN-GA"] > mu["2026-SEN-MI"] > mu["2026-SEN-OH-S"]
    assert np.isfinite(bisect_mu(shocks.margins[:, 0], 1.0))


def _senate_specs(ne_left: str = IND) -> list[RaceSpec]:
    specs = senate_seat_specs([])
    return [
        RaceSpec(s.race_id, s.state, s.region, s.clusters, left_party=ne_left) if s.state == "NE" else s
        for s in specs
    ]


def test_senate_aggregation_counts_seats_correctly() -> None:
    specs = _senate_specs()
    assert len(specs) == len(SENATE_SEATS_UP) == 35
    shocks = NumpySimEngine().simulate(specs, SimParams(), 20_000, seed=1)
    # Everyone holds their seat with certainty => 47 D-caucus seats, never control.
    hold = {s.race_id: (50.0 if SENATE_SEATS_UP[s.state] == "DEM" else -50.0) for s in specs}
    held = apply_mu(shocks, hold)
    np.testing.assert_allclose(senate_dem_seats(held, specs), 47.0)
    assert not senate_dem_control(held, specs).any()
    # Flip exactly four R seats (none NE) to D => 51 => control in every draw.
    flips = ["2026-SEN-ME", "2026-SEN-OH-S", "2026-SEN-TX", "2026-SEN-IA"]
    flipped = apply_mu(shocks, {**hold, **dict.fromkeys(flips, 50.0)})
    assert senate_dem_control(flipped, specs).all()


def test_independent_winner_caucuses_half_the_time() -> None:
    specs = _senate_specs()
    shocks = NumpySimEngine().simulate(specs, SimParams(), 40_000, seed=2)
    hold = {s.race_id: (50.0 if SENATE_SEATS_UP[s.state] == "DEM" else -50.0) for s in specs}
    ne_wins = apply_mu(shocks, {**hold, senate("NE").race_id: 50.0})
    seats = senate_dem_seats(ne_wins, specs, ind_caucus_dem=0.5, seed=9)
    assert seats.mean() == pytest.approx(47.5, abs=0.02)


def test_house_control_is_a_calibrated_pseudo_race() -> None:
    specs = [house_control_spec(), _race("2026-HOUSE-NY-17", "NY", ("house",))]
    shocks = NumpySimEngine().simulate(specs, SimParams(), 50_000, seed=4)
    fitted = apply_mu(shocks, calibrate(shocks, {HOUSE_CONTROL_ID: 0.92}))
    assert house_dem_control(fitted).mean() == pytest.approx(0.92, abs=0.003)
    # Shares N and the house cluster with House seats.
    assert np.corrcoef(fitted.margins.T)[0, 1] > 0.3


def test_win_party_prob_for_independent_race() -> None:
    ne = RaceSpec("2026-SEN-NE", "NE", "midwest", left_party=IND)
    assert ne.win_party_prob(IND, 0.3) == 0.3
    assert ne.win_party_prob(REP, 0.3) == pytest.approx(0.7)
    assert ne.win_party_prob("DEM", 0.3) == 0.0


def test_tourney_passive_field_threshold_is_one() -> None:
    params = FieldParams(n_players=500, frac_kelly=0.0, frac_yolo=0.0, frac_bot=0.0)
    t = simulate_threshold(np.zeros((100, 3), bool), np.array([0.5, 0.2, 0.1]), params)
    np.testing.assert_allclose(t, 1.0)
    assert p_first(np.full(100, 1.5), t) == 1.0
    assert p_first(np.full(100, 1.0), t) == 0.5  # tie split


def test_tourney_single_contract_yolo_matches_analytic() -> None:
    # One contract at p=0.1, huge YOLO crowd => T = 10 exactly when it hits, else Kelly max <= 1.3.
    params = FieldParams(n_players=2000, frac_kelly=0.5, frac_yolo=0.5, frac_bot=0.0)
    hit = np.random.default_rng(0).random((10_000, 1)) < 0.1
    t = simulate_threshold(hit, np.array([0.1]), params, seed=1)
    assert np.all(t[hit[:, 0]] == pytest.approx(10.0))
    assert np.all(t[~hit[:, 0]] <= 1.3)
    assert p_first(np.full(10_000, 12.0), t) == 1.0
    assert p_first(np.full(10_000, 5.0), t) == pytest.approx(1 - hit.mean())


def test_tourney_bigger_field_raises_threshold() -> None:
    rng = np.random.default_rng(5)
    prices = rng.uniform(0.02, 0.98, 60)
    outcomes = rng.random((4000, 60)) < prices
    small = simulate_threshold(outcomes, prices, FieldParams(n_players=300), seed=1)
    big = simulate_threshold(outcomes, prices, FieldParams(n_players=10_000), seed=1)
    assert np.median(big) > np.median(small)


def test_prior_threshold_interpolates_spec_table() -> None:
    assert prior_threshold(2000).p50 == pytest.approx(15.0)
    assert prior_threshold(10_000).p90 == pytest.approx(100.0)
    mid = prior_threshold(1000)
    assert 8.0 < mid.p50 < 15.0
    assert prior_threshold(50).p50 == pytest.approx(8.0)  # clamped
    s = mid.sample(50_000, seed=1)
    assert np.median(s) == pytest.approx(mid.p50, rel=0.03)
    assert np.quantile(s, 0.9) == pytest.approx(mid.p90, rel=0.05)


def test_facade_falls_back_to_numpy_without_native(monkeypatch) -> None:
    monkeypatch.setitem(sys.modules, "susq_models", None)  # import raises ImportError
    assert facade.get_engine().name == "numpy"
    assert facade.get_threshold_simulator()[0] == "numpy"


def test_facade_ignores_native_module_with_wrong_api(monkeypatch) -> None:
    monkeypatch.setitem(sys.modules, "susq_models", types.ModuleType("susq_models"))
    assert facade.get_engine().name == "numpy"


def test_facade_adapts_native_module_with_matching_api(monkeypatch) -> None:
    """A fake C++ module honouring the documented flat-array contract is used and adapted."""
    ref = NumpySimEngine()

    def simulate_margins(mu, beta, race_sd, region_idx, state_idx, membership, cluster_sd, *rest):
        n_draws, seed = rest[-2], rest[-1]
        k = len(mu)
        races = [RaceSpec(f"r{i}", mu=float(mu[i])) for i in range(k)]
        d = ref.simulate(races, SimParams(), n_draws, seed)
        return d.margins, d.national, np.zeros((n_draws, membership.shape[1]))

    fake = types.ModuleType("susq_models")
    fake.SIM_API_VERSION = 1
    fake.simulate_margins = simulate_margins
    monkeypatch.setitem(sys.modules, "susq_models", fake)
    engine = facade.get_engine()
    assert engine.name == "susq_models"
    out = engine.simulate([_race("2026-SEN-TX", "TX", ("red-state-reach",), mu=1.0)], SimParams(), 1000, 1)
    assert out.margins.shape == (1000, 1)
    assert out.engine == "susq_models"
    assert set(out.clusters) == {"red-state-reach"}
