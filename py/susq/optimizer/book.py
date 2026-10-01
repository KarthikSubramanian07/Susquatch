"""Phase-2 scenario book (SPEC §3): pick the scenario that maximizes P(S)·P(M_S > T | S), then
build a book of cheap correlated legs that all pay in S.

For each scenario S:

1. *consistent legs* = legs with P(YES | S) ≥ `min_leg_prob_in_scenario` that are likelier in S than
   overall (lift ≥ `min_lift`), ranked by payoff per SUSQie in S, P(YES | S) / ask — cheap legs
   that S makes likely come first;
2. concentration: for k = 1..`max_legs`, spend `book_frac` of equity (`early_book_frac` before
   `early_until_utc`: the book is bought gradually, full size from Oct 21) on the top-k legs in
   equal cost slices (capped per leg and by book depth) and keep the k with the best P(M > T | S).
   Fewer, cheaper legs are how a book reaches the 10-30× multiples that win (SPEC §6);
3. M_S(ω) = (cash left + Σ qty_j · YES_j(ω)) / 100,000 — the final-balance multiple;
4. crowding: the field's exposure e_S ∈ [−1, 1] inflates the bar, T_S(ω) = T(ω)·(1 + λ·e_S), because
   if the field is loaded on S, more of it finishes high when S happens;
5. score(S) = P(S) · P(M_S > T_S | S). When no book can beat T in any scenario (typical before
   the tails are priced cheaply), scenarios are ranked by the continuous proxy
   P(S) · E[min(M_S / T_S, 1) | S], i.e. "how close to winning", so the choice is never arbitrary.

The chosen book then gets tie hygiene (non-round quantities plus one idiosyncratic leg, since
everyone starts at exactly 100,000), locked flags (late-reporting core legs are held to the lock;
early-reporting ones are election-night rolling fuel) and tranches.
"""

from __future__ import annotations

import hashlib
import math
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field

import numpy as np

from susq.contracts import targets_pb2
from susq.mapping.geo import is_early_reporting
from susq.models.tourney import p_first
from susq.models.types import BoolArray, FloatArray
from susq.optimizer.legs import Leg
from susq.optimizer.scenarios import Scenario
from susq.research_env import OptimizerConfig
from susq.telemetry.crowd import exposure

INITIAL_BALANCE = 100_000.0
TICK = 0.005


@dataclass(frozen=True)
class ScenarioScore:
    scenario: Scenario
    exposure: float
    legs: tuple[int, ...]  # indices into the leg list
    qty: tuple[int, ...]
    p_beat_given_s: float
    score: float
    closeness: float  # P(S) · E[min(M/T, 1) | S], the tie-breaker when every score is 0


@dataclass
class OptimizerResult:
    best: ScenarioScore | None
    ranked: list[ScenarioScore]
    positions: list[targets_pb2.TargetPosition] = field(default_factory=list)
    p_first: float = 0.0
    p_top3: float = 0.0
    book_cost: float = 0.0
    explanation: str = ""


def _floor_tick(p: float) -> float:
    """Round down to the 0.005 tick, returned exactly at pip precision (no 0.41000000000000003)."""
    return round(max(TICK, math.floor(p / TICK + 1e-9) * TICK), 4)


def _allocate(
    legs: Sequence[Leg], idx: Sequence[int], budget: float, equity: float, cfg: OptimizerConfig
) -> list[int]:
    if not idx:
        return []
    per_leg = min(budget / len(idx), cfg.max_leg_frac * equity)
    return [_depth_capped(legs[j], int(per_leg / legs[j].ask)) for j in idx]


def _depth_capped(leg: Leg, qty: int) -> int:
    depth = leg.ask_depth
    return max(0, qty if depth is None else min(qty, int(depth)))


def _multiple(outcomes: BoolArray, idx: Sequence[int], qty: Sequence[int], cash: float) -> FloatArray:
    """Final-balance multiple per world: (cash left + shares that pay) / 100,000."""
    payoff = outcomes[:, list(idx)].astype(np.float64) @ np.asarray(qty, dtype=np.float64)
    return np.asarray((cash + payoff) / INITIAL_BALANCE, dtype=np.float64)


def score_scenarios(
    legs: Sequence[Leg],
    outcomes: BoolArray,
    scenarios: Sequence[Scenario],
    threshold: FloatArray,
    cfg: OptimizerConfig,
    *,
    equity: float = INITIAL_BALANCE,
    cluster_drift_pts: Mapping[str, float] | None = None,
    book_frac: float | None = None,
) -> list[ScenarioScore]:
    """Score every scenario, best first. `book_frac` (default `cfg.book_frac`) is the spend cap."""
    budget = (cfg.book_frac if book_frac is None else book_frac) * equity
    asks = np.array([leg.ask for leg in legs])
    uncond = outcomes.mean(axis=0) if legs else np.zeros(0)
    scored: list[ScenarioScore] = []
    for s in scenarios:
        y_s = outcomes[s.mask]
        cond = y_s.mean(axis=0) if legs else np.zeros(0)
        eligible = np.flatnonzero(
            (cond >= cfg.min_leg_prob_in_scenario) & (cond >= cfg.min_lift * np.maximum(uncond, 1e-9))
        )
        ranked = [int(j) for j in eligible[np.argsort(-(cond[eligible] / asks[eligible]), kind="stable")]]
        e = exposure(s.signs, cluster_drift_pts or {})
        t_s = threshold[s.mask] * (1 + cfg.crowding_lambda * e)
        idx, qty, p_beat, close = _best_concentration(
            legs, y_s, ranked[: cfg.max_legs], t_s, equity, cfg, budget=budget
        )
        scored.append(ScenarioScore(s, e, tuple(idx), tuple(qty), p_beat, s.prob * p_beat, s.prob * close))
    scored.sort(key=lambda x: (-x.score, -x.closeness, x.scenario.scenario_id))
    return scored


def _best_concentration(
    legs: Sequence[Leg],
    y_s: BoolArray,
    ranked: Sequence[int],
    t_s: FloatArray,
    equity: float,
    cfg: OptimizerConfig,
    *,
    budget: float,
) -> tuple[list[int], list[int], float, float]:
    """(legs, qty, P(M > T | S), E[min(M/T, 1) | S]) for the best top-k prefix of `ranked`."""
    best: tuple[list[int], list[int], float, float] = ([], [], 0.0, 0.0)
    for k in range(1, len(ranked) + 1):
        idx = list(ranked[:k])
        qty = _allocate(legs, idx, budget, equity, cfg)
        cost = sum(q * legs[j].ask for j, q in zip(idx, qty, strict=True))
        m = np.broadcast_to(_multiple(y_s, idx, qty, equity - cost), t_s.shape)
        p_beat = p_first(m, t_s)
        close = float(np.minimum(m / t_s, 1.0).mean())
        if (p_beat, close) > (best[2], best[3]):
            best = (idx, qty, p_beat, close)
    return best


def _non_round(qty: int, key: str) -> int:
    """Nudge quantities off multiples of 5 deterministically (tie hygiene)."""
    if qty <= 5 or qty % 5:
        return qty
    return qty - 1 - int(hashlib.sha256(key.encode()).hexdigest(), 16) % 4


def _idiosyncratic_leg(
    legs: Sequence[Leg], outcomes: BoolArray, mask: BoolArray, taken: set[int]
) -> int | None:
    """The mid-priced leg whose odds move least between S and not-S."""
    best, best_gap = None, math.inf
    uncond = outcomes.mean(axis=0)
    cond = outcomes[mask].mean(axis=0)
    for j, leg in enumerate(legs):
        if j in taken or not (0.15 <= leg.ask <= 0.85):
            continue
        gap = abs(float(cond[j] - uncond[j]))
        if gap < best_gap:
            best, best_gap = j, gap
    return best


def build_book(
    legs: Sequence[Leg],
    outcomes: BoolArray,
    scenarios: Sequence[Scenario],
    threshold: FloatArray,
    cfg: OptimizerConfig,
    *,
    equity: float = INITIAL_BALANCE,
    cluster_drift_pts: Mapping[str, float] | None = None,
    book_frac: float | None = None,
) -> OptimizerResult:
    """Score scenarios and turn the best one into target positions.

    `book_frac` is the share of equity the book may spend (callers pass `cfg.book_frac_at(now)` so
    the early, smaller size applies before `early_until_utc`); it defaults to `cfg.book_frac`.
    """
    frac = cfg.book_frac if book_frac is None else book_frac
    ranked = score_scenarios(
        legs,
        outcomes,
        scenarios,
        threshold,
        cfg,
        equity=equity,
        cluster_drift_pts=cluster_drift_pts,
        book_frac=frac,
    )
    best = ranked[0] if ranked else None
    if best is None or not best.legs:
        return OptimizerResult(
            best, ranked, explanation=f"no scenario has any consistent, priced leg (book frac {frac:.0%})"
        )

    s = best.scenario
    cond, uncond = outcomes[s.mask].mean(axis=0), outcomes.mean(axis=0)
    idx = list(best.legs)
    qty = [_non_round(q, legs[j].exchange_id) for j, q in zip(idx, best.qty, strict=True)]
    by_value = sorted(idx, key=lambda j: -(cond[j] / legs[j].ask))
    half = math.ceil(len(by_value) / 2)
    tranche = {j: ("T1" if rank < half else "T2") for rank, j in enumerate(by_value)}

    idio = _idiosyncratic_leg(legs, outcomes, s.mask, set(idx))
    if idio is not None:
        q = int(cfg.idiosyncratic_frac * equity / legs[idio].ask)
        q = _depth_capped(legs[idio], _non_round(max(1, q), f"{legs[idio].exchange_id}/idio"))
        if q > 0:
            idx.append(idio)
            qty.append(q)
            tranche[idio] = "T3"

    positions = [
        _position(legs[j], q, tranche[j], float(cond[j]), float(uncond[j]), s, cfg)
        for j, q in zip(idx, qty, strict=True)
        if q > 0
    ]
    cost = sum(legs[j].ask * q for j, q in zip(idx, qty, strict=True))
    pf = p_first(_multiple(outcomes, idx, qty, equity - cost), threshold)
    return OptimizerResult(
        best=best,
        ranked=ranked,
        positions=positions,
        p_first=pf,
        p_top3=min(1.0, 2.5 * pf),
        book_cost=cost,
        explanation=(
            f"scenario {s.scenario_id} ({s.describe()}): P(S)={s.prob:.3f}, "
            f"P(M>T|S)={best.p_beat_given_s:.3f}, field exposure {best.exposure:+.2f}; "
            f"{len(positions)} legs costing {cost:,.0f} ({cost / equity:.0%} of equity, "
            f"book frac {frac:.0%}); unconditional P(1st)={pf:.4f}"
        ),
    )


def _primary_cluster(leg: Leg) -> str:
    """Named cluster if the leg has one, else its state, else national (chamber markets)."""
    named = [c for c in leg.clusters if c != "national" and not c.startswith(("region:", "state:"))]
    states = [c for c in leg.clusters if c.startswith("state:")]
    return (named or states or ["national"])[0]


def _position(
    leg: Leg, qty: int, tranche: str, p_in_s: float, p_all: float, s: Scenario, cfg: OptimizerConfig
) -> targets_pb2.TargetPosition:
    early = leg.kind != "chamber" and bool(leg.state) and is_early_reporting(leg.state)
    core = tranche != "T3"
    cap = min(leg.ask + cfg.price_slippage_ticks * TICK, max(leg.fair_hi, leg.ask))
    if core:
        role = "early-reporting: election-night rolling fuel" if early else "held to the lock"
        why = (
            f"{s.scenario_id}: pays in S ({s.describe()}) with P={p_in_s:.2f} at ask {leg.ask:.3f} "
            f"({p_in_s / leg.ask:.1f}x payoff per SUSQie in S); fair {leg.fair:.3f}; {role}"
        )
    else:
        why = (
            f"idiosyncratic tie-breaker: the mid-priced leg least tied to S (P={p_in_s:.2f} in S vs "
            f"{p_all:.2f} overall); fair {leg.fair:.3f}"
        )
    return targets_pb2.TargetPosition(
        exchange_id=leg.exchange_id,
        market_id=leg.market_id,
        target_qty=qty,
        max_entry_price=_floor_tick(cap),
        locked=core and not early,
        tranche=tranche,
        cluster=_primary_cluster(leg),
        rationale=why,
    )
