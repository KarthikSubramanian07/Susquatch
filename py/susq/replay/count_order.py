"""The pre-registered count-order playbook (`config/count_order.toml`, EXP-006) for drills.

Mirrors `crates/core/src/count_order.rs` (the executor's reader): per state, the expected partial-count
drift (reported D minus R margin minus final margin, points) at +1, +3, +6, +12 h after poll close and
at the lock, with a band. Linear between points, the first value held before +1 h, linearly to zero
between the lock and `final_h`, zero after; nothing before poll close. A drift has a direction only
when |drift| > band. Overrides are keyed by race-id patterns with `*` wildcards; the pattern with the
most literal characters wins.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from fnmatch import fnmatchcase
from pathlib import Path
from statistics import NormalDist
from typing import Any

from susq.replay.closing import REPO_ROOT

DEFAULT_PATH = REPO_ROOT / "config" / "count_order.toml"
_POINTS = ("h1", "h3", "h6", "h12", "lock")
_PHI = NormalDist()


@dataclass(frozen=True)
class Registered:
    hours_after_close: float
    drift_pts: float
    band_pts: float

    def favours(self) -> str | None:
        """'DEM' / 'REP': which party the count still to come favours (None without a direction)."""
        if abs(self.drift_pts) <= self.band_pts:
            return None
        return "DEM" if self.drift_pts < 0 else "REP"


@dataclass(frozen=True)
class Profile:
    state: str
    key: str
    lock_h: float
    final_h: float
    drift: tuple[float, ...]
    band: tuple[float, ...]

    def at_hour(self, h: float) -> Registered | None:
        if h < 0:
            return None
        knots = (1.0, 3.0, 6.0, 12.0, self.lock_h, self.final_h)
        return Registered(
            h, _interp(knots, (*self.drift, 0.0), h), max(0.0, _interp(knots, (*self.band, 0.0), h))
        )

    def lock(self) -> Registered:
        return Registered(self.lock_h, self.drift[-1], self.band[-1])


def _interp(knots: tuple[float, ...], values: tuple[float, ...], h: float) -> float:
    if h <= knots[0]:
        return values[0]
    for i in range(1, len(knots)):
        if h <= knots[i]:
            x0, x1 = knots[i - 1], knots[i]
            if x1 - x0 <= 1e-12:
                return values[i]
            return values[i - 1] + (h - x0) / (x1 - x0) * (values[i] - values[i - 1])
    return 0.0


def _points(d: dict[str, float]) -> tuple[float, ...]:
    return tuple(float(d[p]) for p in _POINTS)


@dataclass(frozen=True)
class CountOrderTable:
    raw: dict[str, Any]

    @classmethod
    def load(cls, path: Path = DEFAULT_PATH) -> CountOrderTable:
        return cls(tomllib.loads(path.read_text()))

    @property
    def states(self) -> list[str]:
        return sorted(self.raw["states"])

    def profile(self, race_id: str, state: str | None = None) -> Profile | None:
        """The race's registered curve (override, else the state's row); None when unregistered."""
        code = state or (race_id.split("-")[2] if race_id.count("-") >= 2 else "")
        s = self.raw["states"].get(code)
        if s is None:
            return None
        matches = [p for p in s.get("races", {}) if fnmatchcase(race_id, p)]
        best = max(matches, key=lambda p: len(p.replace("*", "")), default=None)
        row = s["races"][best] if best else s
        return Profile(
            state=code,
            key=best or code,
            lock_h=float(s["lock_h"]),
            final_h=float(s["final_h"]),
            drift=_points(row["drift_pts"]),
            band=_points(row["band_pts"]),
        )


def band_prob(margin_mean: float, margin_sd: float, band_pts: float, lean: str) -> float | None:
    """The band in probability for a buy leaning `lean` ('DEM'/'REP'): the move in its win probability
    when the margin shifts `band_pts` against it. None without a usable sd (as in the executor)."""
    if not (margin_sd > 0 and abs(margin_mean) < float("inf")):
        return None
    against = -abs(band_pts) if lean == "DEM" else abs(band_pts)
    return abs(_PHI.cdf(margin_mean / margin_sd) - _PHI.cdf((margin_mean + against) / margin_sd))


__all__ = ["DEFAULT_PATH", "CountOrderTable", "Profile", "Registered", "band_prob"]
