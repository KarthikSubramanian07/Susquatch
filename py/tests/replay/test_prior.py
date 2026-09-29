from __future__ import annotations

import math

import pytest
from susq.contracts import from_json, needle_pb2, to_json
from susq.replay.closing import StateClosing
from susq.replay.history import EDAY, MAIL, MIXED, ContestHistory
from susq.replay.prior import RaceInput, build_prior, weighted_prior_margin


def _prior(contest: ContestHistory, closings: dict[str, StateClosing], **kw: float) -> needle_pb2.NeedlePrior:
    race = RaceInput("R", contest, **kw)  # type: ignore[arg-type]
    return build_prior([race], {contest.state: closings[contest.state].last_close_utc})


def test_shift_reproduces_the_race_prior(
    ga_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    p = _prior(ga_senate, closings, prior_margin_mean=-3.0, prior_margin_sd=4.0)
    # Weighted by historical two-party votes the county priors average exactly to the race prior.
    num = den = 0.0
    for cp in p.counties:
        c = ga_senate.counties[cp.county_fips].tally
        num += cp.prior_margin * c.two_party
        den += c.two_party
    assert num / den == pytest.approx(-3.0, abs=1e-6)
    assert weighted_prior_margin(p, "R") == pytest.approx(-3.0, abs=0.5)


def test_race_prior_fields(ga_senate: ContestHistory, closings: dict[str, StateClosing]) -> None:
    p = _prior(ga_senate, closings, prior_margin_mean=1.0, prior_margin_sd=4.0, national_beta=0.9)
    (race,) = p.races
    assert race.poll_close_utc == "2026-11-04T00:00:00Z"
    assert race.national_beta == pytest.approx(0.9)
    assert len(p.counties) == 159
    assert p.national_shift_prior_sd == pytest.approx(3.0)
    assert all(math.isclose(c.prior_margin_sd, math.hypot(4.0, 4.0)) for c in p.counties)


def test_vote_type_shares_and_offsets(ga_senate: ContestHistory, closings: dict[str, StateClosing]) -> None:
    p = _prior(ga_senate, closings, prior_margin_mean=0.0, prior_margin_sd=5.0)
    fulton = next(c for c in p.counties if c.county_fips == "13121")
    assert sum(v.share_of_total for v in fulton.vote_types) == pytest.approx(1.0)
    by_type = {v.vote_type: v for v in fulton.vote_types}
    assert by_type[MAIL].margin_offset > by_type[EDAY].margin_offset
    assert by_type[MAIL].typical_report_order < by_type[EDAY].typical_report_order


def test_turnout_scaling_and_uncertainty(
    nh_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    p = _prior(nh_senate, closings, prior_margin_mean=2.0, prior_margin_sd=5.0, turnout_ratio=0.8)
    for cp in p.counties:
        hist = nh_senate.counties[cp.county_fips].tally.total
        assert cp.expected_total_votes == pytest.approx(0.8 * hist)
        assert 0.08 <= cp.expected_total_sd_frac <= 0.6
        assert [v.vote_type for v in cp.vote_types] == [MIXED]


def test_json_round_trip(ga_senate: ContestHistory, closings: dict[str, StateClosing]) -> None:
    p = _prior(ga_senate, closings, prior_margin_mean=0.0, prior_margin_sd=5.0)
    again = from_json(needle_pb2.NeedlePrior, to_json(p))
    assert again == p
