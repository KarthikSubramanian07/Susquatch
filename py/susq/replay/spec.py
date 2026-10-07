"""Replay/drill specs: one TOML file names the historical contests, their race ids and the
pre-election priors, so building a replay, building its `NeedlePrior` and running a drill all
read the same inputs.

```toml
history = "data/replay/county_results_2022.csv"   # relative to the spec file or the repo root
# or a list: history = ["data/replay/county_results_2022.csv", "data/replay/county_results_2024.csv"]
seed = 7
horizon_h = 17.0            # 7pm ET close -> noon ET lock
bucket_s = 60.0
national_shift_prior_sd = 3.0

[[races]]
race_id = "2022-SEN-GA"
state = "GA"
year = 2022
office = "US SENATE"
prior_margin_mean = 0.0     # D - R pts, two-party, as the market/sim saw it before the night
prior_margin_sd = 5.0
turnout_ratio = 1.0         # replaying the same election: 1.0
# optional: district = "07" (US House history built with `download --house` or `--house-only`),
# single_unit = true (with a district: model the district as ONE reporting unit keyed "CA-HD07",
#   as California's feed reports it; see `history.collapse_to_unit`),
# poll_close_utc = "2026-11-04T00:00:00Z" (pins the close instead of config/poll_closing.toml)
```
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from susq.contracts import bus_pb2, needle_pb2
from susq.replay.closing import REPO_ROOT, StateClosing, load_closings
from susq.replay.history import ContestKey, collapse_to_unit, district_unit_id, load_all, pool_independent
from susq.replay.prior import DISTRICT_TURNOUT_SD_FRAC, DISTRICT_UNIT_SD_PTS, RaceInput, build_prior
from susq.replay.records import ReplayMeta
from susq.replay.timeline import BuildConfig, RaceSpec, build_replay


@dataclass(frozen=True)
class Spec:
    path: Path
    history: tuple[Path, ...]
    build: BuildConfig
    national_shift_prior_sd: float
    races: tuple[RaceInput, ...]


def _resolve(base: Path, p: str) -> Path:
    for candidate in (base / p, REPO_ROOT / p, Path(p)):
        if candidate.exists():
            return candidate
    raise FileNotFoundError(f"history file {p!r} not found next to the spec or in the repo")


def _parse_utc(s: str) -> datetime:
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=UTC)


def load_spec(path: Path) -> Spec:
    """Parse a spec and load every race's historical contest (each history file is read once).

    An `independent_major` race pools the leading independent with the Democrat in its history
    (`history.pool_independent`), matching how the needle counts that race's non-R side.
    """
    data = tomllib.loads(path.read_text())
    raw = data["history"]
    history = tuple(_resolve(path.parent, h) for h in ([raw] if isinstance(raw, str) else raw))
    build = BuildConfig(
        seed=int(data.get("seed", 7)),
        horizon_s=float(data.get("horizon_h", 17.0)) * 3600.0,
        bucket_s=float(data.get("bucket_s", 60.0)),
    )
    contests = load_all(history)
    races = []
    for r in data["races"]:
        key = ContestKey(
            int(r["year"]),
            r["state"].upper(),
            r["office"].upper(),
            str(r.get("district", "STATEWIDE")).upper(),
        )
        if key not in contests:
            raise ValueError(f"{path}: {r['race_id']}: no rows for {key} in {[str(h) for h in history]}")
        independent = bool(r.get("independent_major", False))
        close = r.get("poll_close_utc")
        contest = pool_independent(contests[key]) if independent else contests[key]
        single_unit = bool(r.get("single_unit", False))
        unit_sd, turnout_sd = 4.0, 0.08  # RaceInput's county defaults
        if single_unit:
            if key.district == "STATEWIDE":
                raise ValueError(f"{path}: {r['race_id']}: single_unit needs a House district")
            unit = district_unit_id(key.state, key.district)
            contest = collapse_to_unit(contest, unit, f"{key.state} district {key.district}")
            unit_sd, turnout_sd = DISTRICT_UNIT_SD_PTS, DISTRICT_TURNOUT_SD_FRAC
        races.append(
            RaceInput(
                race_id=r["race_id"],
                contest=contest,
                prior_margin_mean=float(r["prior_margin_mean"]),
                prior_margin_sd=float(r.get("prior_margin_sd", 5.0)),
                national_beta=float(r.get("national_beta", 1.0)),
                independent_major=independent,
                turnout_ratio=float(r.get("turnout_ratio", 1.0)),
                turnout_sd_frac=turnout_sd,
                county_sd_pts=unit_sd,
                poll_close_utc=_parse_utc(close) if close else None,
                single_unit=single_unit,
            )
        )
    if not races:
        raise ValueError(f"{path}: no [[races]]")
    return Spec(path, history, build, float(data.get("national_shift_prior_sd", 3.0)), tuple(races))


def materialize_prior(spec: Spec, closings: dict[str, StateClosing] | None = None) -> needle_pb2.NeedlePrior:
    """Build only the `NeedlePrior` for a spec (no synthetic timeline)."""
    closings = closings or load_closings()
    return build_prior(
        list(spec.races),
        {
            r.contest.state: closings[r.contest.state].last_close_utc
            for r in spec.races
            if not r.poll_close_utc
        },
        national_shift_prior_sd=spec.national_shift_prior_sd,
    )


def materialize(
    spec: Spec, closings: dict[str, StateClosing] | None = None
) -> tuple[list[bus_pb2.CountyResultsBatch], ReplayMeta, needle_pb2.NeedlePrior]:
    """Build the replay batches, their meta (truth) and the matching `NeedlePrior`."""
    closings = closings or load_closings()
    batches, meta = build_replay([RaceSpec(r.race_id, r.contest) for r in spec.races], closings, spec.build)
    return batches, meta, materialize_prior(spec, closings)
