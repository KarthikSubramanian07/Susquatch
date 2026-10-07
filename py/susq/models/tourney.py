"""Tournament field model (SPEC §6), numpy reference: the winning multiple T per simulated world.

Field of N players split into types:

* passive: finish at 1.0×;
* consensus Kelly: multiples ~ Uniform(0.8, 1.3), world-independent;
* YOLO: all-in on one contract picked with probability ∝ price^power; ends at 1/p_j if it
  resolves YES in that world, else 0;
* bots with election-night compounding: lognormal multiples.

T(ω) is the field maximum. Maxima of iid draws use the order-statistic identity
max = F⁻¹(U^{1/n}) so cost does not grow with N. For YOLO players only "was contract j picked by
at least one of them" matters, drawn as Bernoulli(1 − (1 − w_j)^{n_yolo}) per world (ignoring the
slight negative dependence of multinomial counts).

P(1st) = E[1{M > T} + ½·1{M = T}] per world, averaged over worlds.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np
from scipy import stats

from susq.models.types import BoolArray, FloatArray


@dataclass(frozen=True)
class FieldParams:
    n_players: int = 2000
    frac_kelly: float = 0.35
    frac_yolo: float = 0.15
    frac_bot: float = 0.05  # the rest are passive
    kelly_lo: float = 0.8
    kelly_hi: float = 1.3
    yolo_price_power: float = 1.0
    bot_log_mean: float = math.log(2.0)
    bot_log_sd: float = 0.8

    def count(self, frac: float) -> int:
        return max(0, round(self.n_players * frac))


def consensus_and_bot_max(worlds: int, params: FieldParams, rng: np.random.Generator) -> FloatArray:
    """Per-world maximum of the passive (1.0), consensus-Kelly and bot players, world-independent.

    Draws the Kelly maximum first and the bot maximum second, which is the order
    `simulate_threshold` has always used, so its output is unchanged by the refactor.
    """
    t = np.ones(worlds)
    n_kelly = params.count(params.frac_kelly)
    if n_kelly:
        u = rng.random(worlds) ** (1.0 / n_kelly)
        t = np.maximum(t, params.kelly_lo + (params.kelly_hi - params.kelly_lo) * u)

    n_bot = params.count(params.frac_bot)
    if n_bot:
        u = rng.random(worlds) ** (1.0 / n_bot)
        t = np.maximum(t, np.exp(params.bot_log_mean + params.bot_log_sd * stats.norm.ppf(u)))
    return t


def simulate_threshold(
    outcomes: BoolArray, prices: FloatArray, params: FieldParams, *, seed: int = 0
) -> FloatArray:
    """Winning multiple T(ω) for each world. `outcomes` is (worlds, contracts), `prices` (contracts,)."""
    rng = np.random.default_rng(seed)
    t = consensus_and_bot_max(outcomes.shape[0], params, rng)

    n_yolo = params.count(params.frac_yolo)
    if n_yolo and prices.size:
        p = np.clip(prices, 1e-3, 1.0)
        w = p**params.yolo_price_power
        w = w / w.sum()
        picked = rng.random(outcomes.shape) < (1.0 - (1.0 - w) ** n_yolo)[None, :]
        hits = np.where(picked & outcomes, 1.0 / p[None, :], 0.0)
        t = np.maximum(t, hits.max(axis=1))
    return t


def p_first(my_multiple: FloatArray, threshold: FloatArray) -> float:
    """P(1st) with ties split, averaged over worlds."""
    wins = (my_multiple > threshold).astype(np.float64) + 0.5 * (my_multiple == threshold)
    return float(wins.mean())


@dataclass(frozen=True)
class ThresholdFit:
    """A fitted distribution of the winning multiple (from leaderboard telemetry or the prior)."""

    p50: float
    p90: float
    field_size: int
    method: str

    @property
    def log_sd(self) -> float:
        return max(1e-6, (math.log(self.p90) - math.log(self.p50)) / 1.2815515655446004)

    def sample(self, n: int, *, seed: int = 0) -> FloatArray:
        rng = np.random.default_rng(seed)
        return np.exp(math.log(self.p50) + self.log_sd * rng.standard_normal(n))


# SPEC §6 prior: N≈500 → 8-15×, 2,000 → 15-30×, 10,000 → 25-100×. Read as (p50, p90).
_PRIOR = ((500, 8.0, 15.0), (2000, 15.0, 30.0), (10000, 25.0, 100.0))


def prior_threshold(field_size: int) -> ThresholdFit:
    """Log-log interpolation of the SPEC prior table (clamped at its ends)."""
    n = max(1, field_size)
    xs = [math.log(r[0]) for r in _PRIOR]
    x = min(max(math.log(n), xs[0]), xs[-1])
    p50 = math.exp(float(np.interp(x, xs, [math.log(r[1]) for r in _PRIOR])))
    p90 = math.exp(float(np.interp(x, xs, [math.log(r[2]) for r in _PRIOR])))
    return ThresholdFit(p50, p90, field_size, "prior")
