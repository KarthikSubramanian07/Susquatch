"""`NeedlePrior` builder: historical county results + a race-level prior margin → county priors.

For each race:

* **Race shift.** `shift = prior_margin_mean − M_hist`, where `M_hist` is the historical statewide
  D−R two-party margin. Every county's prior margin is its historical margin plus `shift`, so the
  county priors, weighted by historical two-party votes, reproduce the race prior exactly.
* **Turnout.** `expected_total_votes = historical total × turnout_ratio` (for example ~0.75 when
  the baseline is a presidential year and the target a midterm). The uncertainty fraction combines
  a size term (small counties are noisier) with the turnout-ratio uncertainty in quadrature.
* **Vote types.** Where history splits by vote type, each type's share of the county total and its
  margin offset relative to the county margin (the mail vs E-day skew) come from the data; the
  report order comes from the state's reporting convention. Unsplit history yields one MIXED entry.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from datetime import datetime

from susq.contracts import SCHEMA_VERSION, common_pb2, needle_pb2
from susq.replay.conventions import convention_for
from susq.replay.history import MIXED, ContestHistory, CountyHistory

MIN_TYPE_VOTES_FOR_OFFSET = 50


@dataclass(frozen=True)
class RaceInput:
    """Race-level inputs (from the pre-election sim / markets) for one race."""

    race_id: str
    contest: ContestHistory  # historical baseline for the same state
    prior_margin_mean: float  # D − R, pts, two-party
    prior_margin_sd: float
    national_beta: float = 1.0
    independent_major: bool = False
    turnout_ratio: float = 1.0
    turnout_sd_frac: float = 0.08
    county_sd_pts: float = 4.0


def _size_sd_frac(expected: float) -> float:
    return 0.05 + 1.5 / math.sqrt(max(expected, 1.0))


def _county_prior(r: RaceInput, c: CountyHistory, shift: float, poll_state: str) -> needle_pb2.CountyPrior:
    tally = c.tally
    expected = tally.total * r.turnout_ratio
    sd_frac = min(math.hypot(_size_sd_frac(expected), r.turnout_sd_frac), 0.6)
    margin = max(-100.0, min(100.0, tally.margin_pts() + shift))
    cp = needle_pb2.CountyPrior(
        race_id=r.race_id,
        state=poll_state,
        county_fips=c.county_fips,
        expected_total_votes=expected,
        expected_total_sd_frac=sd_frac,
        prior_margin=margin,
        prior_margin_sd=math.hypot(r.prior_margin_sd, r.county_sd_pts),
    )
    conv = convention_for(poll_state)
    if not c.is_split or tally.total == 0:
        cp.vote_types.add(vote_type=MIXED, share_of_total=1.0, margin_offset=0.0, typical_report_order=0.5)
        return cp
    base = tally.margin_pts()
    for vt in sorted(c.by_type):
        t = c.by_type[vt]
        offset = t.margin_pts() - base if t.two_party >= MIN_TYPE_VOTES_FOR_OFFSET else 0.0
        cp.vote_types.add(
            vote_type=vt,
            share_of_total=t.total / tally.total,
            margin_offset=offset,
            typical_report_order=conv.for_type(vt).report_order,
        )
    return cp


def build_prior(
    races: list[RaceInput],
    poll_close_utc: dict[str, datetime],
    *,
    national_shift_prior_sd: float = 3.0,
    seq: int = 1,
    produced_at_ns: int | None = None,
    producer: str = "python-replay/prior",
) -> needle_pb2.NeedlePrior:
    """Assemble a `NeedlePrior` for `races`. `poll_close_utc` maps state → last poll close."""
    header = common_pb2.Header(
        schema_version=SCHEMA_VERSION,
        produced_at_ns=produced_at_ns if produced_at_ns is not None else 0,
        producer=common_pb2.Producer(name=producer, git_sha="replay"),
        seq=seq,
    )
    prior = needle_pb2.NeedlePrior(header=header, national_shift_prior_sd=national_shift_prior_sd)
    for r in races:
        state = r.contest.state
        shift = r.prior_margin_mean - r.contest.tally.margin_pts()
        prior.races.add(
            race_id=r.race_id,
            state=state,
            prior_margin_mean=r.prior_margin_mean,
            prior_margin_sd=r.prior_margin_sd,
            national_beta=r.national_beta,
            independent_major=r.independent_major,
            poll_close_utc=poll_close_utc[state].strftime("%Y-%m-%dT%H:%M:%SZ"),
        )
        for fips in sorted(r.contest.counties):
            prior.counties.append(_county_prior(r, r.contest.counties[fips], shift, state))
    return prior


def weighted_prior_margin(prior: needle_pb2.NeedlePrior, race_id: str) -> float:
    """Vote-weighted D−R two-party margin implied by the county priors (a consistency check).

    Weights are expected two-party votes, approximated by expected totals (the other-party share is
    assumed uniform across counties).
    """
    num = den = 0.0
    for c in prior.counties:
        if c.race_id == race_id:
            num += c.prior_margin * c.expected_total_votes
            den += c.expected_total_votes
    return num / den if den else 0.0


__all__ = ["RaceInput", "build_prior", "weighted_prior_margin"]
