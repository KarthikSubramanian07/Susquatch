"""Chamber control from race outcomes.

Senate: seats not up (34 D, 31 R) plus the simulated winners of the 35 seats on the ballot;
Democrats control at ≥51 (a 50-50 Senate goes to the Republican VP). An independent winner
(NE, Osborn) caucuses with Democrats with probability `ind_caucus_dem` per draw; he has said he
will caucus with neither, so the default 0.5 is a coin flip, not a claim.

House: without offline district-level partisan lean data, House control is simulated as a single
race-like contract `2026-CONTROL-HOUSE` (national beta 1, the shared "house" cluster, its own
idiosyncratic noise) calibrated to the real-money House mid. That preserves its correlation with
every other race through N while making no claims about seat counts. Individual House seats in
the Cup are separate races with state/region/"house" shocks.
"""

from __future__ import annotations

from collections.abc import Sequence

import numpy as np

from susq.mapping.geo import (
    SENATE_D_CONTROL_SEATS,
    SENATE_D_NOT_UP,
    SENATE_SEATS_UP,
    region_of,
)
from susq.mapping.races import DEM, IND, chamber, senate
from susq.models.types import BoolArray, FloatArray, RaceSpec, SimDraws

HOUSE_CONTROL_ID = chamber("house").race_id
SENATE_CONTROL_ID = chamber("senate").race_id
HOUSE_CONTROL_RACE_SD = 3.0


def senate_seat_specs(existing: Sequence[RaceSpec]) -> list[RaceSpec]:
    """RaceSpecs for every 2026 Senate seat not already in `existing` (μ is calibrated later)."""
    have = {r.race_id for r in existing}
    out = []
    for state in sorted(SENATE_SEATS_UP):
        rid = senate(state).race_id
        if rid not in have:
            out.append(RaceSpec(rid, state, region_of(state), ()))
    return out


def house_control_spec() -> RaceSpec:
    return RaceSpec(HOUSE_CONTROL_ID, "", "", ("house",), race_sd=HOUSE_CONTROL_RACE_SD)


def senate_dem_seats(
    draws: SimDraws, specs: Sequence[RaceSpec], *, ind_caucus_dem: float = 0.5, seed: int = 0
) -> FloatArray:
    """Democratic-caucus seat count per draw."""
    by_id = {s.race_id: s for s in specs}
    seats = np.full(draws.n, float(SENATE_D_NOT_UP))
    rng = np.random.default_rng(seed)
    for state in SENATE_SEATS_UP:
        rid = senate(state).race_id
        if rid not in draws.race_ids:
            raise KeyError(f"senate seat {rid} missing from the simulation")
        left = draws.margins[:, draws.column(rid)] > 0.0
        party = by_id[rid].left_party if rid in by_id else DEM
        if party == IND:
            seats += left & (rng.random(draws.n) < ind_caucus_dem)
        else:
            seats += left
    return seats


def senate_dem_control(
    draws: SimDraws, specs: Sequence[RaceSpec], *, ind_caucus_dem: float = 0.5, seed: int = 0
) -> BoolArray:
    seats = senate_dem_seats(draws, specs, ind_caucus_dem=ind_caucus_dem, seed=seed)
    return np.asarray(seats >= SENATE_D_CONTROL_SEATS)


def house_dem_control(draws: SimDraws) -> BoolArray:
    return draws.margins[:, draws.column(HOUSE_CONTROL_ID)] > 0.0
