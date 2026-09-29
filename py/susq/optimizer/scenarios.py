"""Scenario grid over simulated worlds: national-shift decile × named-cluster shock signs.

A scenario S is a set of worlds. With 10 deciles and 4 named clusters there are 160 cells of
≈ n/160 worlds each. Signs follow the model convention (+1 = the shock favoured Democrats), so
"R beats polls, and the red-state reach collapses" is decile 0-2 with red-state-reach = −1.
"""

from __future__ import annotations

import itertools
from dataclasses import dataclass

import numpy as np

from susq.models.types import BoolArray, SimDraws


@dataclass(frozen=True)
class Scenario:
    scenario_id: str
    decile: int  # 0 = most Republican national environment, 9 = most Democratic
    signs: dict[str, int]  # "national" plus each named cluster -> ±1
    mask: BoolArray

    @property
    def prob(self) -> float:
        return float(self.mask.mean())

    def describe(self) -> str:
        env = "R-leaning" if self.decile <= 3 else ("D-leaning" if self.decile >= 6 else "neutral")
        parts = [f"national decile {self.decile} ({env})"]
        parts += [f"{c} {'D' if s > 0 else 'R'}" for c, s in self.signs.items() if c != "national"]
        return ", ".join(parts)


def scenario_grid(draws: SimDraws, *, deciles: int = 10, min_worlds: int = 50) -> list[Scenario]:
    """Every populated (decile, cluster-sign) cell with at least `min_worlds` worlds."""
    edges = np.quantile(draws.national, np.linspace(0, 1, deciles + 1)[1:-1])
    decile = np.searchsorted(edges, draws.national, side="right")
    names = sorted(draws.clusters)
    positive = {c: draws.clusters[c] > 0.0 for c in names}
    out: list[Scenario] = []
    for d in range(deciles):
        in_decile = decile == d
        for combo in itertools.product((1, -1), repeat=len(names)):
            mask = in_decile.copy()
            for c, s in zip(names, combo, strict=True):
                mask &= positive[c] if s > 0 else ~positive[c]
            if mask.sum() < min_worlds:
                continue
            signs = {"national": 1 if d >= deciles // 2 else -1, **dict(zip(names, combo, strict=True))}
            sid = f"N{d}" + "".join(f"|{c}{'+' if s > 0 else '-'}" for c, s in zip(names, combo, strict=True))
            out.append(Scenario(sid, d, signs, mask))
    return out
