"""Candidate Cup legs and their 0/1 payoff in every simulated world."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass

import numpy as np

from susq.contracts import ops_pb2, targets_pb2
from susq.mapping.races import DEM, REP
from susq.models.chamber import HOUSE_CONTROL_ID, SENATE_CONTROL_ID, house_dem_control, senate_dem_control
from susq.models.types import BoolArray, RaceSpec, SimDraws


@dataclass(frozen=True)
class Leg:
    """A buyable YES contract on the Cup."""

    exchange_id: str
    market_id: str
    title: str
    race_id: str
    party: str
    ask: float  # Cup best ask (YES space)
    ask_depth: float | None  # shares available at/near the ask, if known
    fair: float
    fair_hi: float
    clusters: tuple[str, ...]
    state: str
    kind: str
    verified: bool


def build_legs(
    rules: Sequence[ops_pb2.MarketRule],
    fair: Sequence[targets_pb2.FairValue],
    asks: Mapping[str, tuple[float, float | None]],
    *,
    require_verified: bool = True,
) -> list[Leg]:
    """Join map rows, fair values and Cup asks into legs (rows missing any of them are skipped)."""
    fair_by = {f.exchange_id: f for f in fair}
    out: list[Leg] = []
    for r in rules:
        if not r.race_id or not r.party or (require_verified and not r.verified):
            continue
        f = fair_by.get(r.exchange_id)
        quote = asks.get(r.exchange_id)
        if f is None or quote is None or not (0.0 < quote[0] < 1.0):
            continue
        state = next((c.split(":", 1)[1] for c in r.clusters if c.startswith("state:")), "")
        out.append(
            Leg(
                r.exchange_id, r.market_id, r.title, r.race_id, r.party, quote[0], quote[1],
                f.p_yes, f.p_hi, tuple(r.clusters), state, r.kind, r.verified,
            )
        )  # fmt: skip
    return out


def leg_outcomes(
    draws: SimDraws, specs: Sequence[RaceSpec], legs: Sequence[Leg]
) -> tuple[BoolArray, list[Leg]]:
    """(worlds × legs) YES payoffs, dropping legs whose race is not simulated."""
    by_id = {s.race_id: s for s in specs}
    cols: list[BoolArray] = []
    kept: list[Leg] = []
    senate: BoolArray | None = None
    for leg in legs:
        if leg.kind == "chamber" and leg.party not in (DEM, REP):
            continue
        if leg.race_id == SENATE_CONTROL_ID:
            if senate is None:
                senate = senate_dem_control(draws, specs)
            d_wins = senate
        elif leg.race_id == HOUSE_CONTROL_ID and HOUSE_CONTROL_ID in draws.race_ids:
            d_wins = house_dem_control(draws)
        elif leg.race_id in draws.race_ids and leg.race_id in by_id:
            left = draws.margins[:, draws.column(leg.race_id)] > 0.0
            spec = by_id[leg.race_id]
            if leg.party == spec.left_party:
                cols.append(left)
            elif leg.party == REP:
                cols.append(~left)
            else:
                cols.append(np.zeros(draws.n, dtype=bool))
            kept.append(leg)
            continue
        else:
            continue
        cols.append(d_wins if leg.party == DEM else ~d_wins)
        kept.append(leg)
    matrix = np.column_stack(cols) if cols else np.zeros((draws.n, 0), dtype=bool)
    return matrix, kept
