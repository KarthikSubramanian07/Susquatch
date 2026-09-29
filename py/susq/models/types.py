"""Value types and the `SimEngine` protocol shared by the numpy reference and the C++ engine.

Sign convention everywhere: a race margin is (left-side candidate − Republican) in points of
the two-way vote, so a positive national shift N is a Democratic-leaning environment. The
"left side" is the Democrat except where an independent carries the anti-R vote (NE Senate).
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from types import MappingProxyType
from typing import Protocol

import numpy as np
import numpy.typing as npt

from susq.mapping.races import DEM, REP

FloatArray = npt.NDArray[np.float64]
BoolArray = npt.NDArray[np.bool_]

DEFAULT_CLUSTER_SD: Mapping[str, float] = MappingProxyType(
    {"red-state-reach": 1.5, "new-england-incumbency": 1.5, "midwest-industrial": 1.5, "house": 1.5}
)


@dataclass(frozen=True)
class RaceSpec:
    """One simulated contest. `state`/`region` are empty for chamber pseudo-races."""

    race_id: str
    state: str = ""
    region: str = ""
    clusters: tuple[str, ...] = ()  # named clusters only (not national/region/state)
    mu: float = 0.0  # mean left-side margin, points
    race_sd: float = 3.0  # idiosyncratic SD (larger with sparse polls or independents)
    national_beta: float = 1.0  # loading on the national shift N
    left_party: str = DEM

    def win_party_prob(self, party: str, left_win: float) -> float:
        """P(party wins) given P(left side wins); third parties get 0 (market-only)."""
        if party == self.left_party:
            return left_win
        if party == REP:
            return 1.0 - left_win
        return 0.0


@dataclass(frozen=True)
class SimParams:
    """SPEC §5 parameterization (points of margin)."""

    nu: float = 5.0  # Student-t degrees of freedom for N
    national_sd: float = 2.75
    region_sd: float = 1.5
    state_sd: float = 2.5
    miss_p: float = 0.10  # systematic-miss mixture weight
    miss_shift: float = 3.5  # extra ±shift of N in a miss
    cluster_sd: Mapping[str, float] = field(default_factory=lambda: dict(DEFAULT_CLUSTER_SD))

    def __post_init__(self) -> None:
        if self.nu <= 2:
            raise ValueError("nu must exceed 2 for a finite variance")


@dataclass
class SimDraws:
    """Joint draws. `margins[:, i]` belongs to `race_ids[i]`; shocks are exposed for scenarios."""

    race_ids: tuple[str, ...]
    margins: FloatArray  # (n, k)
    national: FloatArray  # (n,) total national shift including the systematic miss
    clusters: dict[str, FloatArray]  # named cluster shocks, (n,) each
    engine: str = "numpy"

    @property
    def n(self) -> int:
        return int(self.margins.shape[0])

    def column(self, race_id: str) -> int:
        return self.race_ids.index(race_id)

    def left_wins(self) -> BoolArray:
        return self.margins > 0.0

    def with_mu(self, mu: Sequence[float] | FloatArray) -> SimDraws:
        """Shift every race by `mu` (common random numbers: shocks are reused, not redrawn)."""
        return SimDraws(
            self.race_ids,
            self.margins + np.asarray(mu, dtype=np.float64)[None, :],
            self.national,
            self.clusters,
            self.engine,
        )


class SimEngine(Protocol):
    """Anything that can draw correlated race margins for a set of RaceSpecs."""

    name: str

    def simulate(self, races: Sequence[RaceSpec], params: SimParams, n_draws: int, seed: int) -> SimDraws:
        """Draw margins including each race's `mu`."""
        ...
