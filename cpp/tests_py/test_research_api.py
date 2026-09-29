"""The flat-array API that py/susq/models/facade.py looks for (SIM/TOURNEY_API_VERSION 1)."""

from __future__ import annotations

import numpy as np
import pytest
import susq_models
from reference_sim import RefRace, RefShocks, analytic_covariance


def test_versions_are_advertised() -> None:
    assert susq_models.SIM_API_VERSION == 1
    assert susq_models.TOURNEY_API_VERSION == 1


def test_simulate_margins_matches_the_closed_form_structure() -> None:
    n = 200_000
    # Races 0, 1: OH (state 0, Midwest = region 0); race 2: GA (state 1, South = region 1);
    # race 3: a chamber pseudo-race with no region/state; races 0 and 2 share cluster 0.
    mu = np.array([1.0, -2.0, 0.5, 3.0])
    beta = np.array([1.0, 1.0, 0.8, 1.0])
    race_sd = np.array([3.0, 3.5, 3.0, 3.0])
    region_idx = np.array([0, 0, 1, -1], dtype=np.int32)
    state_idx = np.array([0, 0, 1, -1], dtype=np.int32)
    membership = np.array([[1.0], [0.0], [1.0], [0.0]])
    cluster_sd = np.array([2.0])
    margins, national, clusters = susq_models.simulate_margins(
        mu, beta, race_sd, region_idx, state_idx, membership, cluster_sd,
        2, 2, 5.0, 2.75, 1.5, 2.5, 0.1, 3.5, n, 7,
    )  # fmt: skip
    assert margins.shape == (n, 4)
    assert national.shape == (n,)
    assert clusters.shape == (n, 1)
    np.testing.assert_allclose(margins.mean(axis=0), mu, atol=0.05)
    races = [
        RefRace("OH", "Midwest", mu=1.0, loadings={"c": 1.0}),
        RefRace("OH", "Midwest", mu=-2.0, sd=3.5),
        RefRace("GA", "South", mu=0.5, beta=0.8, loadings={"c": 1.0}),
    ]
    expected = analytic_covariance(races, RefShocks(), {"c": 2.0})
    np.testing.assert_allclose(np.cov(margins[:, :3].T), expected, atol=0.35, rtol=0.03)
    # The pseudo-race carries only national + its own noise.
    vn = expected[1, 1] - 1.5**2 - 2.5**2 - 3.5**2
    assert np.var(margins[:, 3]) == pytest.approx(vn + 9.0, rel=0.03)
    assert np.var(clusters[:, 0]) == pytest.approx(4.0, rel=0.02)


def test_simulate_margins_is_deterministic_in_seed() -> None:
    args = (
        np.zeros(2),
        np.ones(2),
        np.full(2, 3.0),
        np.array([0, 1]),
        np.array([0, 1]),
        np.zeros((2, 0)),
        np.zeros(0),
    )
    a = susq_models.simulate_margins(*args, 2, 2, 5.0, 2.75, 1.5, 2.5, 0.1, 3.5, 5000, 3)
    b = susq_models.simulate_margins(*args, 2, 2, 5.0, 2.75, 1.5, 2.5, 0.1, 3.5, 5000, 3)
    np.testing.assert_array_equal(a[0], b[0])


def test_simulate_threshold_shape_and_floor() -> None:
    rng = np.random.default_rng(0)
    prices = np.array([0.02, 0.1, 0.4, 0.9])
    outcomes = (rng.random((10_000, 4)) < prices).astype(np.uint8)
    t = susq_models.simulate_threshold(
        outcomes, prices, 2000, 0.35, 0.15, 0.05, 0.8, 1.3, 1.0, np.log(2.0), 0.8, 1
    )
    assert t.shape == (10_000,)
    assert (t >= 1.0).all()
    # When the 2-cent contract pays, T = 50 iff one of the 300 YOLOs picked it.
    w = prices[0] / prices.sum()
    expected = 1 - (1 - w) ** 300
    assert np.mean(t[outcomes[:, 0] == 1] == 50.0) == pytest.approx(expected, abs=0.02)


def test_research_facade_picks_the_native_engine() -> None:
    facade = pytest.importorskip("susq.models.facade")
    assert facade.get_engine().name == "susq_models"
    assert facade.get_threshold_simulator()[0] == "susq_models"
