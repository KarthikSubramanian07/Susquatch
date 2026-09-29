"""Fit each race's mean margin μ so the simulated win probability equals a target.

Common random numbers: the shocks are drawn once with μ = 0, and for each race we solve

    mean(shock_i + μ_i > 0) = p_i

by bisection. The left side is monotone in μ, so bisection converges on the empirical
(1−p)-quantile of −shock; we bisect rather than take the quantile directly so the tolerance is
explicit and identical to what the C++ port does. Because the shocks are shared, recalibrating
after a price move changes only μ, never the correlation structure.
"""

from __future__ import annotations

from collections.abc import Mapping

import numpy as np

from susq.models.types import FloatArray, SimDraws

P_CLIP = (0.005, 0.995)  # a 0/1 target has no finite μ; tick-size bounds are close enough


def bisect_mu(shock: FloatArray, target: float, *, tol: float = 1e-4, max_iter: int = 80) -> float:
    """μ such that mean(shock + μ > 0) ≈ target (|error| ≤ one draw's weight or `tol` in μ)."""
    p = float(np.clip(target, *P_CLIP))
    lo = float(-shock.max()) - 1.0  # P(win) = 0 here
    hi = float(-shock.min()) + 1.0  # P(win) = 1 here
    for _ in range(max_iter):
        mid = 0.5 * (lo + hi)
        if np.mean(shock + mid > 0.0) < p:
            lo = mid
        else:
            hi = mid
        if hi - lo < tol:
            break
    return 0.5 * (lo + hi)


def calibrate(shocks: SimDraws, targets: Mapping[str, float]) -> dict[str, float]:
    """μ per race id present in both `shocks` and `targets` (left-side win probabilities)."""
    out: dict[str, float] = {}
    for race_id, p in targets.items():
        if race_id in shocks.race_ids:
            out[race_id] = bisect_mu(shocks.margins[:, shocks.column(race_id)], p)
    return out


def apply_mu(
    shocks: SimDraws, mu: Mapping[str, float], default: Mapping[str, float] | None = None
) -> SimDraws:
    """Margins = shocks + μ, taking μ from `mu`, then `default`, then 0."""
    vec = np.array(
        [mu.get(r, (default or {}).get(r, 0.0)) for r in shocks.race_ids],
        dtype=np.float64,
    )
    return shocks.with_mu(vec)
