"""The reference needle recovers the truth on synthetic nights and starts from the prior."""

from __future__ import annotations

import math

import pytest
from susq.contracts import SCHEMA_VERSION, bus_pb2, common_pb2
from susq.replay.closing import StateClosing
from susq.replay.history import ContestHistory
from susq.replay.needle_ref import ReferenceNeedle
from susq.replay.prior import RaceInput, build_prior
from susq.replay.timeline import BuildConfig, RaceSpec, build_replay


def _run(contest: ContestHistory, closings: dict[str, StateClosing], prior_mean: float, seed: int = 7):
    batches, meta = build_replay([RaceSpec("R", contest)], closings, BuildConfig(seed=seed))
    prior = build_prior(
        [RaceInput("R", contest, prior_margin_mean=prior_mean, prior_margin_sd=5.0)],
        {contest.state: closings[contest.state].last_close_utc},
    )
    needle = ReferenceNeedle()
    needle.load_prior(prior)
    posts = [needle.ingest(b).races[0] for b in batches]
    return posts, meta.races["R"]


@pytest.mark.parametrize("seed", [1, 2, 3])
def test_recovers_a_close_race_by_end_of_night(
    seed: int, ga_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    posts, truth = _run(ga_senate, closings, prior_mean=-2.0, seed=seed)
    final = posts[-1]
    night_margin = 100 * (truth.night_dem - truth.night_rep) / (truth.night_dem + truth.night_rep)
    assert final.margin_mean == pytest.approx(truth.margin_pts, abs=0.3)
    assert abs(final.margin_mean - night_margin) < 0.3
    assert final.p_dem > 0.9  # Warnock led by ~1 pt of two-party vote
    assert final.reported_frac > 0.97


def test_uncertainty_shrinks_as_counties_report(
    ga_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    posts, _ = _run(ga_senate, closings, prior_mean=0.0)
    sds = [p.margin_sd for p in posts]
    assert sds[-1] < sds[0] / 5
    assert posts[-1].n_counties_reporting == 159
    for p in posts:
        assert 0.0 <= p.p_dem <= 1.0
        assert math.isfinite(p.margin_mean)
        assert p.margin_sd > 0
        assert p.p_dem + p.p_rep == pytest.approx(1.0)


def test_no_data_returns_the_prior(nh_senate: ContestHistory, closings: dict[str, StateClosing]) -> None:
    prior = build_prior(
        [RaceInput("R", nh_senate, prior_margin_mean=3.0, prior_margin_sd=5.0)],
        {"NH": closings["NH"].last_close_utc},
    )
    n = ReferenceNeedle()
    n.load_prior(prior)
    empty = bus_pb2.CountyResultsBatch(header=common_pb2.Header(schema_version=SCHEMA_VERSION, seq=1))
    (post,) = n.ingest(empty).races
    assert post.margin_mean == pytest.approx(3.0, abs=0.3)
    assert post.reported_frac == 0.0
    assert post.n_counties_reporting == 0


def test_ignores_unknown_races_and_counties(
    nh_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    prior = build_prior(
        [RaceInput("R", nh_senate, prior_margin_mean=3.0, prior_margin_sd=5.0)],
        {"NH": closings["NH"].last_close_utc},
    )
    n = ReferenceNeedle()
    n.load_prior(prior)
    b = bus_pb2.CountyResultsBatch(header=common_pb2.Header(schema_version=SCHEMA_VERSION, seq=1))
    r = b.results.add(race_id="OTHER", county_fips="33001", vote_type=bus_pb2.VOTE_TYPE_MIXED)
    r.votes.add(candidate="x", party="DEM", votes=1000)
    r2 = b.results.add(race_id="R", county_fips="99999", vote_type=bus_pb2.VOTE_TYPE_MIXED)
    r2.votes.add(candidate="x", party="DEM", votes=1000)
    (post,) = n.ingest(b).races
    assert post.n_counties_reporting == 0
