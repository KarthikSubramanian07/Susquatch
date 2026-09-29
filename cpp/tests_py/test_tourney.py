"""susq_models.tourney: analytic cases, sim -> tourney plumbing, and fitting."""

from __future__ import annotations

import numpy as np
import pytest
from susq_models import sim, tourney


def only_yolo(n_players: int) -> tourney.FieldParams:
    return tourney.FieldParams(n_players=n_players, f_passive=0.0, f_kelly=0.0, f_yolo=1.0, f_bot=0.0)


def test_everyone_on_one_longshot_ties_with_you() -> None:
    n = 40
    field = tourney.FieldModel(np.array([0.1]), np.ones((1, 1), dtype=np.uint8), only_yolo(n))
    res = tourney.evaluate_book(field, cash=0.0, contracts=[0], shares=[1_000_000.0])
    assert res["multiple"][0] == pytest.approx(10.0)
    assert res["p_first"] == pytest.approx(1 / (n + 1))
    assert res["expected_prize"] == pytest.approx(37_500 / (n + 1))
    assert tourney.threshold_quantiles(field, [0.5])[0] == pytest.approx(10.0)


def test_prize_share_matches_enumeration() -> None:
    # Two others: above w.p. a, tied w.p. t.
    a, t = 0.25, 0.25
    prizes = [30_000.0, 5_000.0, 2_500.0]
    b = 1 - a - t
    expected = (
        b * b * 30_000
        + 2 * b * t * (35_000 / 2)
        + t * t * (37_500 / 3)
        + 2 * a * b * 5_000
        + 2 * a * t * (7_500 / 2)
        + a * a * 2_500
    )
    assert tourney.expected_prize_share(2, a, t, prizes) == pytest.approx(expected)


def test_sim_worlds_feed_the_field_model() -> None:
    cfg = sim.default_config()
    cfg.n_draws = 20_000
    targets = {"OH": 0.58, "IA": 0.43, "TX": 0.61, "KS": 0.30, "SC": 0.12}
    cfg.clusters = [sim.ClusterSpec(name="red-state-reach", sd=2.0)]
    cfg.races = [
        sim.RaceSpec(
            id=s,
            state=s,
            target_p_win=p,
            clusters=[sim.ClusterLoading(cluster="red-state-reach", loading=1.0)],
        )
        for s, p in targets.items()
    ]
    model = sim.Model(cfg)
    model.run()
    # One YES contract per party per race, priced at the calibrated probabilities.
    dem_pays = model.outcomes == sim.DEM_SIDE_WINS
    rep_pays = model.outcomes == sim.REP_WINS
    pays = np.vstack([dem_pays, rep_pays]).astype(np.uint8)
    prices = np.clip(np.concatenate([model.p_dem_market, model.p_rep_market]), 0.01, 0.99)
    field = tourney.FieldModel(prices, pays, tourney.FieldParams(n_players=500))
    # All-in on the correlated red-state sweep is only possible one leg at a time here: take SC-D.
    sc = list(targets).index("SC")
    res = tourney.evaluate_book(field, cash=0.0, contracts=[sc], shares=[100_000 / prices[sc]])
    assert 0.0 <= res["p_first"] <= 1.0
    assert res["p_first"] <= res["p_top3"] <= 1.0
    assert res["expected_prize"] <= 30_000
    np.testing.assert_allclose(np.mean(res["multiple"] > 0), model.p_dem_market[sc], atol=1e-12)
    cash = tourney.evaluate_book(field, cash=100_000.0, contracts=[], shares=[])
    assert cash["p_first"] < 1e-6  # 1.0x never wins against YOLOs


def test_fit_recovers_yolo_share() -> None:
    rng = np.random.default_rng(3)
    truth = tourney.FieldParams(n_players=3000, f_passive=0.5, f_kelly=0.33, f_yolo=0.12, f_bot=0.05)
    prices = 0.01 + 0.01 * np.arange(30)
    snapshots = []
    for _ in range(4):
        values = np.where(rng.random(30) < prices * 1.5, 1 / prices, 0.0)
        types = rng.choice(4, size=truth.n_players, p=[0.5, 0.33, 0.12, 0.05])
        weights = prices / prices[prices <= 0.25].sum() * (prices <= 0.25)
        picks = rng.choice(30, size=truth.n_players, p=weights)
        bots = np.where(rng.random((truth.n_players, 3)) < 0.4, 2.5, 0.0).prod(axis=1)
        mult = np.select(
            [types == 0, types == 1, types == 2],
            [np.ones(truth.n_players), rng.uniform(0.8, 1.3, truth.n_players), values[picks]],
            bots,
        )
        snapshots.append(
            tourney.LeaderboardSnapshot(
                n_players=truth.n_players,
                top_multiples=sorted(mult, reverse=True)[:60],
                contract_values=values.tolist(),
                entry_prices=prices.tolist(),
                bots_active=True,
            )
        )
    base = tourney.FieldParams(n_players=3000, f_passive=0.4, f_kelly=0.264, f_yolo=0.296, f_bot=0.04)
    fit = tourney.fit_field(snapshots, base)
    assert fit["params"].f_yolo == pytest.approx(0.12, abs=0.04)
    assert fit["loss"] == pytest.approx(fit["loss_by_f_yolo"].min())


def test_bad_inputs_raise_value_error() -> None:
    with pytest.raises(ValueError, match="sum to 1"):
        tourney.FieldModel(np.array([0.1]), np.ones((1, 1)), tourney.FieldParams(f_passive=0.9))
    with pytest.raises(ValueError, match="n_contracts"):
        tourney.FieldModel(np.array([0.1, 0.2]), np.ones((1, 3)), tourney.FieldParams())
