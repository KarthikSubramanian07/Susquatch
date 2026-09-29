"""Replay/drill specs: one TOML file names the historical contests, their race ids and the
pre-election priors, so building a replay, building its `NeedlePrior` and running a drill all
read the same inputs.

```toml
history = "data/replay/county_results_2022.csv"   # relative to the spec file or the repo root
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
```
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path

from susq.contracts import bus_pb2, needle_pb2
from susq.replay.closing import REPO_ROOT, StateClosing, load_closings
from susq.replay.history import ContestHistory, load_contest
from susq.replay.prior import RaceInput, build_prior
from susq.replay.records import ReplayMeta
from susq.replay.timeline import BuildConfig, RaceSpec, build_replay


@dataclass(frozen=True)
class Spec:
    path: Path
    history: Path
    build: BuildConfig
    national_shift_prior_sd: float
    races: tuple[RaceInput, ...]


def _resolve(base: Path, p: str) -> Path:
    for candidate in (base / p, REPO_ROOT / p, Path(p)):
        if candidate.exists():
            return candidate
    raise FileNotFoundError(f"history file {p!r} not found next to the spec or in the repo")


def load_spec(path: Path) -> Spec:
    data = tomllib.loads(path.read_text())
    history = _resolve(path.parent, data["history"])
    build = BuildConfig(
        seed=int(data.get("seed", 7)),
        horizon_s=float(data.get("horizon_h", 17.0)) * 3600.0,
        bucket_s=float(data.get("bucket_s", 60.0)),
    )
    contests: dict[tuple[int, str, str], ContestHistory] = {}
    races = []
    for r in data["races"]:
        key = (int(r["year"]), r["state"].upper(), r["office"].upper())
        if key not in contests:
            contests[key] = load_contest(history, year=key[0], state=key[1], office=key[2])
        races.append(
            RaceInput(
                race_id=r["race_id"],
                contest=contests[key],
                prior_margin_mean=float(r["prior_margin_mean"]),
                prior_margin_sd=float(r.get("prior_margin_sd", 5.0)),
                national_beta=float(r.get("national_beta", 1.0)),
                independent_major=bool(r.get("independent_major", False)),
                turnout_ratio=float(r.get("turnout_ratio", 1.0)),
            )
        )
    if not races:
        raise ValueError(f"{path}: no [[races]]")
    return Spec(path, history, build, float(data.get("national_shift_prior_sd", 3.0)), tuple(races))


def materialize(
    spec: Spec, closings: dict[str, StateClosing] | None = None
) -> tuple[list[bus_pb2.CountyResultsBatch], ReplayMeta, needle_pb2.NeedlePrior]:
    """Build the replay batches, their meta (truth) and the matching `NeedlePrior`."""
    closings = closings or load_closings()
    batches, meta = build_replay([RaceSpec(r.race_id, r.contest) for r in spec.races], closings, spec.build)
    prior = build_prior(
        list(spec.races),
        {r.contest.state: closings[r.contest.state].last_close_utc for r in spec.races},
        national_shift_prior_sd=spec.national_shift_prior_sd,
    )
    return batches, meta, prior
