"""Real-money consensus: one probability per (race, party) from all venue quotes.

Venue mids are averaged with weights 1/max(spread, 1c): a tight Kalshi book counts more than a
wide Polymarket one. Stale quotes are dropped. The half-spread and the venue disagreement feed
the uncertainty used for the fair-value band.
"""

from __future__ import annotations

import time
from collections import defaultdict
from collections.abc import Iterable
from dataclasses import dataclass

from susq.collectors.quotes import RefQuote
from susq.mapping.races import DEM, IND, REP

MIN_SPREAD = 0.01


@dataclass(frozen=True)
class Consensus:
    race_id: str
    party: str
    p: float
    sigma: float  # 1-sigma uncertainty in probability units
    venues: tuple[str, ...]
    ts_ns: int  # oldest contributing quote


def consensus(
    quotes: Iterable[RefQuote], *, now_ns: int | None = None, max_age_s: float | None = None
) -> dict[tuple[str, str], Consensus]:
    now = time.time_ns() if now_ns is None else now_ns
    groups: dict[tuple[str, str], list[RefQuote]] = defaultdict(list)
    for q in quotes:
        if max_age_s is not None and now - q.ts_ns > max_age_s * 1e9:
            continue
        groups[(q.race_id, q.party)].append(q)
    out: dict[tuple[str, str], Consensus] = {}
    for key, qs in groups.items():
        weights = [1.0 / max(q.spread, MIN_SPREAD) for q in qs]
        total = sum(weights)
        p = sum(w * q.mid for w, q in zip(weights, qs, strict=True)) / total
        half_spread = sum(w * q.spread / 2 for w, q in zip(weights, qs, strict=True)) / total
        disagreement = (max(q.mid for q in qs) - min(q.mid for q in qs)) / 2
        sigma = max(half_spread, disagreement, MIN_SPREAD / 2)
        out[key] = Consensus(
            key[0], key[1], p, sigma, tuple(sorted({q.venue for q in qs})), min(q.ts_ns for q in qs)
        )
    return out


def left_party(race_id: str, cons: dict[tuple[str, str], Consensus]) -> str:
    """The anti-Republican side: an independent if it out-prices the Democrat, else the Democrat."""
    ind, dem = cons.get((race_id, IND)), cons.get((race_id, DEM))
    if ind is not None and (dem is None or ind.p > dem.p):
        return IND
    return DEM


def left_target(race_id: str, cons: dict[tuple[str, str], Consensus]) -> float | None:
    """P(left side beats the Republican) with the two-way overround removed; None if unpriced."""
    left = cons.get((race_id, left_party(race_id, cons)))
    rep = cons.get((race_id, REP))
    if left is not None and rep is not None and left.p + rep.p > 0:
        return left.p / (left.p + rep.p)
    if left is not None:
        return left.p
    if rep is not None:
        return 1.0 - rep.p
    return None
