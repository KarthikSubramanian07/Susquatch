"""EXP-012: the concentrated longshot book, scored on P(1st) against a world-coupled threshold.

The scenario book (`book.py`) spreads a share of equity over legs that pay together in one scenario cell.
Against the resolution-adjusted field (the Oct 25 edge hunt: the leaders are longshot-selling bots whose
10-11x is mostly realised value, so the bar is about 12-14x in nearly every world; EXP-011's rival model
finds the same order) no such book reaches the bar, so its P(1st) is 0. The only pre-election books
with P(1st) > 0 put nearly the whole balance on one or two cheap legs: M = f / ask if the leg pays,
which clears ~13x at asks of about 0.07 or less.

`best_concentrated` searches

* single legs with ask <= `max_ask`, at each spend fraction in `fracs` (the rest stays cash), and
* equal-cost pairs among the `pair_pool` best singles,

scoring each candidate by `p_first(M, T)` over every simulated world, where T is the per-world field
maximum. It never looks at marks and never sells; it only decides which YES legs to hold.

Modes (`[optimizer.concentrate] mode`): "off" (default) does nothing; "report" adds the best concentrated
book and its P(1st) to the targets notes; "commit" replaces the scenario book's positions with the
concentrated book when its P(1st) beats the scenario book's by at least `min_gain`. Committing only
makes sense with the trader's `[risk] per_market_gross_frac` and cluster caps raised for that leg; with
the current 5% per-market cap the bridge would stop buying at 5% of equity.
"""

from __future__ import annotations

import math
from collections.abc import Sequence
from dataclasses import dataclass

import numpy as np

from susq.contracts import targets_pb2
from susq.mapping.geo import is_early_reporting
from susq.models.tourney import p_first
from susq.models.types import BoolArray, FloatArray
from susq.optimizer.legs import Leg
from susq.research_env import ConcentrateConfig

INITIAL_BALANCE = 100_000.0
TICK = 0.005


@dataclass(frozen=True)
class ConcentratedBook:
    legs: tuple[int, ...]  # indices into the leg list
    qty: tuple[int, ...]
    cost: float
    p_first: float
    p_beat: float  # P(M > T), ties excluded
    multiple_if_pays: float  # the book's best-case multiple

    def describe(self, legs: Sequence[Leg]) -> str:
        names = " + ".join(f"{legs[j].race_id} {legs[j].party} @ {legs[j].ask:.3f}" for j in self.legs)
        return (
            f"concentrated book [{names}]: cost {self.cost:,.0f}, best case {self.multiple_if_pays:.1f}x, "
            f"unconditional P(1st)={self.p_first:.4f}"
        )


def _qty(leg: Leg, spend: float) -> int:
    q = int(spend / leg.ask) if leg.ask > 0 else 0
    return max(0, q if leg.ask_depth is None else min(q, int(leg.ask_depth)))


def _multiple(outcomes: BoolArray, idx: Sequence[int], qty: Sequence[int], cash: float) -> FloatArray:
    payoff = outcomes[:, list(idx)].astype(np.float64) @ np.asarray(qty, dtype=np.float64)
    return np.asarray((cash + payoff) / INITIAL_BALANCE, dtype=np.float64)


def _score(
    legs: Sequence[Leg],
    outcomes: BoolArray,
    threshold: FloatArray,
    idx: Sequence[int],
    spends: Sequence[float],
    equity: float,
) -> ConcentratedBook | None:
    qty = [_qty(legs[j], s) for j, s in zip(idx, spends, strict=True)]
    if not all(qty):
        return None
    cost = sum(q * legs[j].ask for j, q in zip(idx, qty, strict=True))
    m = _multiple(outcomes, idx, qty, equity - cost)
    pf = p_first(m, threshold)
    return ConcentratedBook(tuple(idx), tuple(qty), cost, pf, float((m > threshold).mean()), float(m.max()))


def best_concentrated(
    legs: Sequence[Leg],
    outcomes: BoolArray,
    threshold: FloatArray,
    cfg: ConcentrateConfig,
    *,
    equity: float = INITIAL_BALANCE,
) -> ConcentratedBook | None:
    """The single leg or equal-cost pair with the highest unconditional P(1st); None if nothing qualifies."""
    cheap = [j for j, leg in enumerate(legs) if 0.0 < leg.ask <= cfg.max_ask]
    singles: list[ConcentratedBook] = []
    for j in cheap:
        for f in cfg.fracs:
            b = _score(legs, outcomes, threshold, [j], [f * equity], equity)
            if b is not None:
                singles.append(b)
    if not singles:
        return None
    singles.sort(key=lambda b: (-b.p_first, -b.multiple_if_pays, legs[b.legs[0]].exchange_id))
    pool: list[int] = []
    for b in singles:
        if b.legs[0] not in pool:
            pool.append(b.legs[0])
        if len(pool) >= cfg.pair_pool:
            break
    best = singles[0]
    top_frac = max(cfg.fracs)
    for a in range(len(pool)):
        for c in range(a + 1, len(pool)):
            half = top_frac * equity / 2
            b = _score(legs, outcomes, threshold, [pool[a], pool[c]], [half, half], equity)
            if b is not None and (b.p_first, b.multiple_if_pays) > (best.p_first, best.multiple_if_pays):
                best = b
    return best


def _floor_tick(p: float) -> float:
    return round(math.floor(p / TICK + 1e-9) * TICK, 4)


def concentrated_positions(
    book: ConcentratedBook, legs: Sequence[Leg], cfg: ConcentrateConfig
) -> list[targets_pb2.TargetPosition]:
    """Positions for a committed concentrated book: tranche T1, held to the lock unless early-reporting."""
    out = []
    for j, q in zip(book.legs, book.qty, strict=True):
        leg = legs[j]
        early = leg.kind != "chamber" and bool(leg.state) and is_early_reporting(leg.state)
        cap = min(leg.ask + cfg.price_slippage_ticks * TICK, max(leg.fair_hi, leg.ask))
        out.append(
            targets_pb2.TargetPosition(
                exchange_id=leg.exchange_id,
                market_id=leg.market_id,
                target_qty=q,
                max_entry_price=_floor_tick(cap),
                locked=not early,
                tranche="T1",
                cluster=next((c for c in leg.clusters if c.startswith("state:")), "national"),
                rationale=(
                    f"EXP-012 concentrated book: {q} YES at ask {leg.ask:.3f} (fair {leg.fair:.3f}); "
                    f"book P(1st)={book.p_first:.4f}, best case {book.multiple_if_pays:.1f}x; "
                    + ("early-reporting: election-night exit or roll" if early else "held to the lock")
                ),
            )
        )
    return out
