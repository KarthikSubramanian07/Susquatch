"""Pure-numpy reference implementation of the SPEC §5 correlated race simulator.

    margin_i = mu_i + beta_i·N + R[region(i)] + S[state(i)] + Σ_c C_c·1{i∈c} + ε_i

    N   = national_sd · t_ν · sqrt((ν−2)/ν)  +  miss,   miss = ±miss_shift w.p. miss_p else 0
    R   ~ Normal(0, region_sd)   per region (northeast / midwest / south / west)
    S   ~ Normal(0, state_sd)    per state, shared by every race in the state
    C_c ~ Normal(0, cluster_sd)  per named cluster (red-state reach, ...)
    ε_i ~ Normal(0, race_sd_i)

The t draw is rescaled to unit variance so `national_sd` really is the SD of the non-miss part.
This is the executable spec the C++ `sim` must match; it is fast enough for 10^5-10^6 draws on
a laptop, just not as fast.
"""

from __future__ import annotations

from collections.abc import Sequence

import numpy as np

from susq.models.types import FloatArray, RaceSpec, SimDraws, SimParams


class NumpySimEngine:
    """Reference `SimEngine`."""

    name = "numpy"

    def __init__(self, *, chunk: int = 50_000) -> None:
        self._chunk = chunk

    def simulate(self, races: Sequence[RaceSpec], params: SimParams, n_draws: int, seed: int) -> SimDraws:
        if n_draws <= 0:
            raise ValueError("n_draws must be positive")
        rng = np.random.default_rng(seed)
        regions = sorted({r.region for r in races if r.region})
        states = sorted({r.state for r in races if r.state})
        named = sorted({c for r in races for c in r.clusters})
        k = len(races)

        mu = np.array([r.mu for r in races], dtype=np.float64)
        beta = np.array([r.national_beta for r in races], dtype=np.float64)
        race_sd = np.array([r.race_sd for r in races], dtype=np.float64)
        region_idx = np.array([regions.index(r.region) if r.region else -1 for r in races])
        state_idx = np.array([states.index(r.state) if r.state else -1 for r in races])
        membership = np.array([[c in r.clusters for c in named] for r in races], dtype=np.float64).reshape(
            k, len(named)
        )
        cluster_sd = np.array([params.cluster_sd.get(c, 0.0) for c in named], dtype=np.float64)

        margins = np.empty((n_draws, k), dtype=np.float64)
        national = np.empty(n_draws, dtype=np.float64)
        cluster_draws = np.empty((n_draws, len(named)), dtype=np.float64)
        scale_t = params.national_sd * np.sqrt((params.nu - 2.0) / params.nu)

        for start in range(0, n_draws, self._chunk):
            m = min(self._chunk, n_draws - start)
            t = rng.standard_t(params.nu, size=m) * scale_t
            miss = (rng.random(m) < params.miss_p) * rng.choice([-1.0, 1.0], size=m) * params.miss_shift
            n_shift = t + miss
            reg = rng.normal(0.0, params.region_sd, size=(m, max(len(regions), 1)))
            sta = rng.normal(0.0, params.state_sd, size=(m, max(len(states), 1)))
            clu = rng.normal(0.0, 1.0, size=(m, len(named))) * cluster_sd[None, :]
            eps = rng.normal(0.0, 1.0, size=(m, k)) * race_sd[None, :]

            block = mu[None, :] + beta[None, :] * n_shift[:, None] + eps
            block += np.where(region_idx >= 0, reg[:, np.clip(region_idx, 0, None)], 0.0)
            block += np.where(state_idx >= 0, sta[:, np.clip(state_idx, 0, None)], 0.0)
            block += clu @ membership.T
            margins[start : start + m] = block
            national[start : start + m] = n_shift
            cluster_draws[start : start + m] = clu

        clusters: dict[str, FloatArray] = {c: cluster_draws[:, j].copy() for j, c in enumerate(named)}
        return SimDraws(tuple(r.race_id for r in races), margins, national, clusters, self.name)


def analytic_margin_sd(race: RaceSpec, params: SimParams) -> float:
    """Unconditional SD of a race margin implied by the parameters (used to test the engine)."""
    var_n = params.national_sd**2 + params.miss_p * params.miss_shift**2
    var = race.national_beta**2 * var_n + race.race_sd**2
    var += params.region_sd**2 if race.region else 0.0
    var += params.state_sd**2 if race.state else 0.0
    var += sum(params.cluster_sd.get(c, 0.0) ** 2 for c in race.clusters)
    return float(np.sqrt(var))
