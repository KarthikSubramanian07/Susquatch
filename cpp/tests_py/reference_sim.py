"""Pure-numpy oracle for the sim's marginal and correlation structure (docs/MODELS.md §1).

Independent re-implementation with numpy's RNG: same distributions, different random numbers.
Used to check the C++ simulator's marginals (Monte Carlo agreement) and its covariance
(closed form) without sharing any code with it.
"""

from __future__ import annotations

from dataclasses import dataclass, field

import numpy as np


@dataclass
class RefRace:
    """One race: margin = mu + beta*N + R + S + sum(loading*C) + sd*eps (+ third-party draws)."""

    state: str
    region: str
    mu: float
    sd: float = 3.0
    beta: float = 1.0
    loadings: dict[str, float] = field(default_factory=dict)
    p_third_party: float = 0.0


@dataclass
class RefShocks:
    national_sd: float = 2.75
    national_nu: float = 5.0
    region_sd: float = 1.5
    state_sd: float = 2.5
    miss_prob: float = 0.1
    miss_size: float = 3.5
    miss_prob_dem: float = 0.5


def national_variance(s: RefShocks) -> float:
    """Var(N) for the scaled Student-t plus the systematic-miss mixture."""
    mean_shift = s.miss_prob * (2.0 * s.miss_prob_dem - 1.0) * s.miss_size
    return s.national_sd**2 + s.miss_prob * s.miss_size**2 - mean_shift**2


def analytic_covariance(races: list[RefRace], shocks: RefShocks, cluster_sd: dict[str, float]) -> np.ndarray:
    """Covariance of the race errors e_i (margin minus mu) implied by the model."""
    n = len(races)
    cov = np.zeros((n, n))
    vn = national_variance(shocks)
    for i, a in enumerate(races):
        for j, b in enumerate(races):
            c = a.beta * b.beta * vn
            if a.region == b.region:
                c += shocks.region_sd**2
            if a.state == b.state:
                c += shocks.state_sd**2
            for name, sd in cluster_sd.items():
                c += a.loadings.get(name, 0.0) * b.loadings.get(name, 0.0) * sd**2
            if i == j:
                c += a.sd**2
            cov[i, j] = c
    return cov


def simulate(
    races: list[RefRace],
    shocks: RefShocks,
    cluster_sd: dict[str, float],
    n_draws: int,
    seed: int,
) -> np.ndarray:
    """Outcome codes (n_races, n_draws): 0 = R wins, 1 = D side wins, 2 = third party."""
    rng = np.random.default_rng(seed)
    t_scale = shocks.national_sd * np.sqrt((shocks.national_nu - 2.0) / shocks.national_nu)
    national = t_scale * rng.standard_t(shocks.national_nu, n_draws)
    miss = rng.random(n_draws) < shocks.miss_prob
    sign = np.where(rng.random(n_draws) < shocks.miss_prob_dem, 1.0, -1.0)
    national += miss * sign * shocks.miss_size
    regions = {r.region: shocks.region_sd * rng.standard_normal(n_draws) for r in races}
    states = {r.state: shocks.state_sd * rng.standard_normal(n_draws) for r in races}
    clusters = {name: sd * rng.standard_normal(n_draws) for name, sd in cluster_sd.items()}
    out = np.empty((len(races), n_draws), dtype=np.uint8)
    for i, r in enumerate(races):
        margin = (
            r.mu
            + r.beta * national
            + regions[r.region]
            + states[r.state]
            + r.sd * rng.standard_normal(n_draws)
        )
        for name, w in r.loadings.items():
            margin += w * clusters[name]
        code = (margin > 0).astype(np.uint8)
        if r.p_third_party > 0:
            code[rng.random(n_draws) < r.p_third_party] = 2
        out[i] = code
    return out
