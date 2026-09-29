"""Reference election-night estimator in pure Python.

This is **not** the production needle (that is the C++ `needle` service, reachable from Python
through `susq_models` when built). It exists so replays and drills run end to end without the C++
build, and as an independent cross-check of the C++ numbers. It implements the method in
RESEARCH_cup.md §7 in its simplest defensible form:

1. **Race shift δ.** Each reporting county gives a residual: observed two-party margin minus the
   margin its prior expects *for the vote types reported so far* (prior margin + per-type offset).
   Residuals are combined with the race prior δ ~ N(0, race_sd²) by precision weighting. A
   residual's variance is the county's idiosyncratic variance plus a partial-count term that decays
   as the county completes (early batches are unrepresentative).
2. **Remaining vote.** Per county and vote type, remaining = expected − reported (never negative).
   Its margin is the county prior + type offset + δ, plus half of the county's own residual beyond δ
   (counted and uncounted votes in one county are correlated), scaled by how complete the county is.
3. **Race margin** = (reported D−R + projected remaining D−R) / projected two-party total, with
   variance from δ, county idiosyncrasies and turnout; P(D) = Φ(mean / sd).

The national shift (updating West-coast races from East-coast results) is the C++ needle's job
and is not modeled here.
"""

from __future__ import annotations

import math
from dataclasses import dataclass, field
from statistics import NormalDist

from susq.contracts import SCHEMA_VERSION, bus_pb2, common_pb2, needle_pb2
from susq.replay.history import MIXED

_PHI = NormalDist()
PARTIAL_COUNT_SD = 10.0  # pts: extra residual noise of a county with almost nothing counted
DEFAULT_TWO_PARTY_FRACTION = 0.97


@dataclass
class _Reported:
    dem: int = 0
    rep: int = 0
    total: int = 0


@dataclass
class _RaceState:
    prior: needle_pb2.RacePrior
    counties: dict[str, needle_pb2.CountyPrior] = field(default_factory=dict)
    latest: dict[tuple[str, int], _Reported] = field(default_factory=dict)
    n_batches: int = 0


class ReferenceNeedle:
    """Implements the `NeedleBackend` protocol (see `backends.py`)."""

    name = "python-reference"

    def __init__(self, producer: str = "python-replay/needle-ref") -> None:
        self._races: dict[str, _RaceState] = {}
        self._producer = producer
        self._seq = 0

    def load_prior(self, prior: needle_pb2.NeedlePrior) -> None:
        self._races = {rp.race_id: _RaceState(prior=rp) for rp in prior.races}
        for cp in prior.counties:
            if cp.race_id in self._races:
                self._races[cp.race_id].counties[cp.county_fips] = cp

    def ingest(self, batch: bus_pb2.CountyResultsBatch) -> bus_pb2.RacePosteriors:
        for r in batch.results:
            race = self._races.get(r.race_id)
            if race is None or r.county_fips not in race.counties:
                continue
            dem = sum(c.votes for c in r.votes if c.party == "DEM")
            rep = sum(c.votes for c in r.votes if c.party == "REP")
            race.latest[(r.county_fips, r.vote_type)] = _Reported(dem, rep, r.total_votes)
            race.n_batches += 1
        self._seq += 1
        out = bus_pb2.RacePosteriors(
            header=common_pb2.Header(
                schema_version=SCHEMA_VERSION,
                produced_at_ns=batch.header.produced_at_ns,
                producer=common_pb2.Producer(name=self._producer, git_sha="replay"),
                seq=self._seq,
            )
        )
        for race_id in sorted(self._races):
            out.races.append(self.posterior(race_id))
        return out

    def posterior(self, race_id: str) -> bus_pb2.RacePosterior:
        race = self._races[race_id]
        race_sd = max(race.prior.prior_margin_sd, 0.5)
        by_county: dict[str, dict[int, _Reported]] = {}
        for (fips, vt), rep in race.latest.items():
            by_county.setdefault(fips, {})[vt] = rep

        # 1. Race shift.
        precision = 1.0 / race_sd**2
        weighted = 0.0
        residuals: dict[str, tuple[float, float]] = {}  # fips -> (residual, completeness)
        for fips, types in by_county.items():
            cp = race.counties[fips]
            d = sum(t.dem for t in types.values())
            r = sum(t.rep for t in types.values())
            n = sum(t.total for t in types.values())
            if d + r < 30:
                continue
            offsets: dict[int, float] = {int(v.vote_type): v.margin_offset for v in cp.vote_types}
            expected_margin = sum(
                (t.dem + t.rep) * (cp.prior_margin + offsets.get(vt, 0.0)) for vt, t in types.items()
            ) / (d + r)
            observed = 100.0 * (d - r) / (d + r)
            completeness = min(1.0, n / (cp.expected_total_votes + 1.0))
            county_sd = _county_sd(cp, race_sd)
            pi = 1.0 / (county_sd**2 + PARTIAL_COUNT_SD**2 * (1.0 - completeness))
            precision += pi
            weighted += pi * (observed - expected_margin)
            residuals[fips] = (observed - expected_margin, completeness)
        delta = weighted / precision
        var_delta = 1.0 / precision

        # 2. Remaining vote and 3. race margin.
        rep_d = rep_r = 0
        net_remaining = tp_remaining = 0.0
        expected_all = reported_all = remaining_all = 0.0
        county_terms: list[tuple[float, float, float, float]] = []  # (rem2p, margin, county_sd, sd_frac)
        for fips, cp in race.counties.items():
            types = by_county.get(fips, {})
            d = sum(t.dem for t in types.values())
            r = sum(t.rep for t in types.values())
            n = sum(t.total for t in types.values())
            rep_d += d
            rep_r += r
            expected_all += cp.expected_total_votes
            reported_all += n
            tp_frac = (d + r) / n if n > 100 else DEFAULT_TWO_PARTY_FRACTION
            resid, completeness = residuals.get(fips, (delta, 0.0))
            rho = 0.5 * completeness
            county_sd = _county_sd(cp, race_sd)
            for vt, rem in _remaining_by_type(cp, types).items():
                if rem <= 0:
                    continue
                offset = next((v.margin_offset for v in cp.vote_types if v.vote_type == vt), 0.0)
                m = cp.prior_margin + offset + delta + rho * (resid - delta)
                m = max(-100.0, min(100.0, m))
                rem2p = rem * tp_frac
                net_remaining += rem2p * m / 100.0
                tp_remaining += rem2p
                remaining_all += rem
                county_terms.append((rem2p, m, county_sd * (1.0 - rho), cp.expected_total_sd_frac))

        tp_total = rep_d + rep_r + tp_remaining
        if tp_total <= 0:
            mean = race.prior.prior_margin_mean
            sd = race_sd
        else:
            mean = 100.0 * ((rep_d - rep_r) + net_remaining) / tp_total
            share_remaining = tp_remaining / tp_total
            var = share_remaining**2 * var_delta
            for rem2p, m, csd, sdf in county_terms:
                w = rem2p / tp_total
                var += (w * csd) ** 2 + (w * (m - mean) * sdf) ** 2
            sd = math.sqrt(var + 0.01)
        p_dem = _PHI.cdf(mean / sd)
        return bus_pb2.RacePosterior(
            race_id=race_id,
            p_dem=p_dem,
            p_rep=1.0 - p_dem,
            margin_mean=mean,
            margin_sd=sd,
            expected_remaining_votes=remaining_all,
            reported_frac=min(1.0, reported_all / expected_all) if expected_all else 0.0,
            n_batches=race.n_batches,
            n_counties_reporting=len(by_county),
        )


def _county_sd(cp: needle_pb2.CountyPrior, race_sd: float) -> float:
    """County idiosyncratic sd: the part of the county prior sd not explained by the race sd."""
    return math.sqrt(max(cp.prior_margin_sd**2 - race_sd**2, 1.0))


def _remaining_by_type(cp: needle_pb2.CountyPrior, types: dict[int, _Reported]) -> dict[int, float]:
    """Expected-but-unreported votes per vote type.

    If every reported type is one the prior knows, subtract per type. Otherwise (for example the
    source reports MIXED but the prior is split) subtract the county total and spread the rest by
    the prior's type shares.
    """
    shares = {v.vote_type: v.share_of_total for v in cp.vote_types} or {MIXED: 1.0}
    expected = cp.expected_total_votes
    if set(types) <= set(shares):
        return {
            vt: max(expected * s - (types[vt].total if vt in types else 0), 0.0) for vt, s in shares.items()
        }
    left = max(expected - sum(t.total for t in types.values()), 0.0)
    return {vt: left * s for vt, s in shares.items()}
