"""Synthetic election-night reporting timelines from final county results.

For every county and vote type, the final votes are released as a few cumulative batches:

* the first batch lands `first_report_min` after the state's last poll close (per the state's
  convention), scaled by county size (`(votes / median)^0.25`, so big counties are slower) with
  log-normal noise; later batches follow at `spread_min` intervals, again noisy;
* each batch's candidate mix varies around the type's final mix (a per-candidate Dirichlet around
  shared batch fractions), so early batches can mislead, but the cumulative total always lands
  exactly on the real result;
* only `night_completion` of each type is counted on the night; the rest (late mail, provisional)
  arrives 20-72 h later, usually beyond the replay horizon, which is what makes late-count races
  hard at the Nov 4 lock.

Events are grouped into `bucket_s` windows (the poller cadence), one `CountyResultsBatch` per
window, in the replay file format described in `records.py`.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass
from datetime import datetime
from typing import cast

import numpy as np

from susq.contracts import SCHEMA_VERSION, bus_pb2, common_pb2
from susq.replay.closing import StateClosing
from susq.replay.conventions import convention_for
from susq.replay.history import ContestHistory, CountyHistory
from susq.replay.records import RaceTruth, ReplayMeta, batch_id

NS = 1_000_000_000


@dataclass(frozen=True)
class RaceSpec:
    """Which historical contest to replay, and under which race id."""

    race_id: str
    contest: ContestHistory


@dataclass(frozen=True)
class BuildConfig:
    seed: int = 7
    horizon_s: float = 17 * 3600.0  # 7pm ET close → noon ET lock
    bucket_s: float = 60.0
    time_noise: float = 0.35
    composition_concentration: float = 150.0
    max_results_per_batch: int = 500
    producer: str = "python-replay"
    git_sha: str = "replay"


@dataclass
class _Event:
    t_s: float
    race_id: str
    state: str
    fips: str
    vote_type: int
    votes: dict[str, tuple[str, int]]  # candidate -> (party code, cumulative votes)


def _party(medsl: str) -> str:
    return {"DEMOCRAT": "DEM", "REPUBLICAN": "REP", "LIBERTARIAN": "LIB", "INDEPENDENT": "IND"}.get(
        medsl, "OTH"
    )


def _allocate(rng: np.random.Generator, final: dict[str, int], n: int, conc: float) -> list[dict[str, int]]:
    """Split each candidate's `final` into `n` cumulative steps with correlated, noisy fractions."""
    mean = rng.dirichlet(np.full(n, 2.0))
    out: list[dict[str, int]] = [{} for _ in range(n)]
    for cand, total in final.items():
        frac = rng.dirichlet(conc * mean + 1e-3) if n > 1 else np.ones(1)
        cum = np.floor(np.cumsum(frac) * total).astype(np.int64)
        cum[-1] = total
        cum = np.maximum.accumulate(np.minimum(cum, total))
        for k in range(n):
            out[k][cand] = int(cum[k])
    return out


def _county_events(
    rng: np.random.Generator,
    race_id: str,
    county: CountyHistory,
    offset_s: float,
    size_factor: float,
    cfg: BuildConfig,
) -> list[_Event]:
    conv = convention_for(county.state)
    events: list[_Event] = []
    parties = {cand: _party(p) for cand, (p, _) in county.candidates.items()}
    small = county.tally.total < 5_000
    for vt in sorted(county.by_type):
        final = {cand: per.get(vt, 0) for cand, (_, per) in county.candidates.items()}
        if sum(final.values()) == 0:
            continue
        tc = conv.for_type(vt)
        night = {c: round(v * tc.night_completion) for c, v in final.items()}
        lo, hi = tc.batches
        if small:
            hi = min(hi, 2)
            lo = min(lo, hi)
        n = int(rng.integers(lo, hi + 1))
        times = [offset_s + 60.0 * tc.first_report_min * size_factor * rng.lognormal(0.0, cfg.time_noise)]
        for _ in range(n - 1):
            times.append(times[-1] + 60.0 * tc.spread_min * size_factor * rng.lognormal(0.0, 0.5))
        if sum(night.values()) > 0:
            for t, cum in zip(times, _allocate(rng, night, n, cfg.composition_concentration), strict=True):
                votes = {c: (parties[c], cum[c]) for c in final}
                events.append(_Event(t, race_id, county.state, county.county_fips, vt, votes))
        if any(final[c] > night[c] for c in final):
            t_late = offset_s + 3600.0 * rng.uniform(20.0, 72.0)
            votes = {c: (parties[c], final[c]) for c in final}
            events.append(
                _Event(max(t_late, times[-1] + 60.0), race_id, county.state, county.county_fips, vt, votes)
            )
    return events


def _result(e: _Event) -> bus_pb2.CountyResult:
    r = bus_pb2.CountyResult(
        race_id=e.race_id,
        state=e.state,
        county_fips=e.fips,
        source=f"synthetic:{e.state}",
        source_ts_ns=int(e.t_s * NS),
        vote_type=cast("bus_pb2.VoteType", e.vote_type),  # stub enum type; value is a valid VoteType int
    )
    for cand in sorted(e.votes):
        party, v = e.votes[cand]
        r.votes.add(candidate=cand, party=party, votes=v)
    r.total_votes = sum(v for _, v in e.votes.values())
    r.batch_id = batch_id(r)
    return r


def build_replay(
    races: Sequence[RaceSpec],
    closings: dict[str, StateClosing],
    cfg: BuildConfig | None = None,
) -> tuple[list[bus_pb2.CountyResultsBatch], ReplayMeta]:
    """Build the batches and meta (truth + provenance) for a set of historical contests."""
    cfg = cfg or BuildConfig()
    if not races:
        raise ValueError("no races")
    rng = np.random.default_rng(cfg.seed)
    close: dict[str, datetime] = {r.contest.state: closings[r.contest.state].last_close_utc for r in races}
    anchor = min(close.values())
    events: list[_Event] = []
    truths: dict[str, RaceTruth] = {}
    for spec in races:
        c = spec.contest
        offset_s = (close[c.state] - anchor).total_seconds()
        totals = np.array([x.tally.total for x in c.counties.values()], dtype=float)
        median = float(np.median(totals[totals > 0])) if (totals > 0).any() else 1.0
        race_events: list[_Event] = []
        for fips in sorted(c.counties):
            county = c.counties[fips]
            size_factor = float(np.clip((max(county.tally.total, 1) / median) ** 0.25, 0.6, 2.5))
            race_events.extend(_county_events(rng, spec.race_id, county, offset_s, size_factor, cfg))
        events.extend(race_events)
        final = c.tally
        night_dem, night_rep = _night_two_party(race_events, cfg.horizon_s)
        truths[spec.race_id] = RaceTruth(
            race_id=spec.race_id,
            state=c.state,
            dem=final.dem,
            rep=final.rep,
            other=final.other,
            night_dem=night_dem,
            night_rep=night_rep,
            poll_close_offset_s=offset_s,
        )
    events.sort(key=lambda e: (e.t_s, e.race_id, e.fips, e.vote_type))
    batches = _bucket(events, cfg)
    meta = ReplayMeta(
        anchor_utc=anchor.strftime("%Y-%m-%dT%H:%M:%SZ"),
        seed=cfg.seed,
        horizon_s=cfg.horizon_s,
        races=truths,
        inputs=[f"{r.contest.year} {r.contest.state} {r.contest.office} -> {r.race_id}" for r in races],
    )
    return batches, meta


def _night_two_party(events: list[_Event], horizon_s: float) -> tuple[int, int]:
    latest: dict[tuple[str, int], dict[str, tuple[str, int]]] = {}
    for e in sorted(events, key=lambda e: e.t_s):
        if e.t_s <= horizon_s:
            latest[(e.fips, e.vote_type)] = e.votes
    dem = sum(v for votes in latest.values() for p, v in votes.values() if p == "DEM")
    rep = sum(v for votes in latest.values() for p, v in votes.values() if p == "REP")
    return dem, rep


def _bucket(events: list[_Event], cfg: BuildConfig) -> list[bus_pb2.CountyResultsBatch]:
    """Group events into poller-cadence windows; drop anything past the horizon."""
    out: list[bus_pb2.CountyResultsBatch] = []
    seq = 0
    current: list[_Event] = []
    current_bucket: int | None = None

    def flush() -> None:
        nonlocal seq
        if not current or current_bucket is None:
            return
        emit_ns = int((current_bucket + 1) * cfg.bucket_s * NS)
        for i in range(0, len(current), cfg.max_results_per_batch):
            seq += 1
            b = bus_pb2.CountyResultsBatch(
                header=common_pb2.Header(
                    schema_version=SCHEMA_VERSION,
                    produced_at_ns=emit_ns,
                    producer=common_pb2.Producer(name=cfg.producer, git_sha=cfg.git_sha),
                    seq=seq,
                )
            )
            for e in current[i : i + cfg.max_results_per_batch]:
                r = _result(e)
                r.observed_at_ns = emit_ns
                b.results.append(r)
            out.append(b)

    for e in events:
        if e.t_s > cfg.horizon_s:
            break
        bucket = int(max(e.t_s, 0.0) // cfg.bucket_s)
        if bucket != current_bucket:
            flush()
            current = []
            current_bucket = bucket
        current.append(e)
    flush()
    return out
