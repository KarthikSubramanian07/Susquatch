"""Choose a `SimEngine`: the C++ `susq_models` extension when present, else the numpy reference.

The C++ module is built separately (pybind11, `feat/cpp-models`). To be picked up it must expose

    SIM_API_VERSION = 1
    simulate_margins(mu, beta, race_sd, region_idx, state_idx, membership, cluster_sd,
                     n_regions, n_states, nu, national_sd, region_sd, state_sd,
                     miss_p, miss_shift, n_draws, seed)
        -> (margins[n, k] float64, national[n] float64, clusters[n, c] float64)

with exactly the semantics of `numpy_engine.NumpySimEngine` (index -1 = "no region/state").
Anything else (missing module, other version, import error) falls back to numpy, and the chosen
engine's name is recorded in every target file so a run is always attributable.
"""

from __future__ import annotations

import importlib
import logging
from collections.abc import Callable, Sequence
from types import ModuleType

import numpy as np

from susq.models.numpy_engine import NumpySimEngine
from susq.models.tourney import FieldParams, simulate_threshold
from susq.models.types import BoolArray, FloatArray, RaceSpec, SimDraws, SimEngine, SimParams

log = logging.getLogger(__name__)

NATIVE_MODULE = "susq_models"
SIM_API_VERSION = 1


class NativeSimEngine:
    """Adapter from the flat-array C++ API to `SimEngine`."""

    name = "susq_models"

    def __init__(self, module: ModuleType) -> None:
        self._m = module

    def simulate(self, races: Sequence[RaceSpec], params: SimParams, n_draws: int, seed: int) -> SimDraws:
        regions = sorted({r.region for r in races if r.region})
        states = sorted({r.state for r in races if r.state})
        named = sorted({c for r in races for c in r.clusters})
        margins, national, clusters = self._m.simulate_margins(
            np.array([r.mu for r in races], dtype=np.float64),
            np.array([r.national_beta for r in races], dtype=np.float64),
            np.array([r.race_sd for r in races], dtype=np.float64),
            np.array([regions.index(r.region) if r.region else -1 for r in races], dtype=np.int32),
            np.array([states.index(r.state) if r.state else -1 for r in races], dtype=np.int32),
            np.array([[c in r.clusters for c in named] for r in races], dtype=np.float64).reshape(
                len(races), len(named)
            ),
            np.array([params.cluster_sd.get(c, 0.0) for c in named], dtype=np.float64),
            len(regions),
            len(states),
            params.nu,
            params.national_sd,
            params.region_sd,
            params.state_sd,
            params.miss_p,
            params.miss_shift,
            n_draws,
            seed,
        )
        margins = np.asarray(margins, dtype=np.float64)
        clusters = np.asarray(clusters, dtype=np.float64)
        if margins.shape != (n_draws, len(races)):
            raise ValueError(f"susq_models returned margins of shape {margins.shape}")
        return SimDraws(
            tuple(r.race_id for r in races),
            margins,
            np.asarray(national, dtype=np.float64),
            {c: clusters[:, j].copy() for j, c in enumerate(named)},
            self.name,
        )


def get_engine(*, prefer_native: bool = True) -> SimEngine:
    """The best available engine (never raises)."""
    module = _import_native() if prefer_native else None
    if module is None:
        return NumpySimEngine()
    if getattr(module, "SIM_API_VERSION", None) == SIM_API_VERSION and hasattr(module, "simulate_margins"):
        return NativeSimEngine(module)
    log.warning("susq_models found but does not expose SIM_API_VERSION=1; using numpy")
    return NumpySimEngine()


ThresholdFn = Callable[[BoolArray, FloatArray, FieldParams, int], FloatArray]


def get_threshold_simulator(*, prefer_native: bool = True) -> tuple[str, ThresholdFn]:
    """(name, fn) computing T(ω) per world; native if `susq_models.TOURNEY_API_VERSION == 1`.

    Native contract: `simulate_threshold(outcomes[n, j] uint8, prices[j] float64, n_players,
    frac_kelly, frac_yolo, frac_bot, kelly_lo, kelly_hi, yolo_price_power, bot_log_mean,
    bot_log_sd, seed) -> T[n] float64`, semantics of `tourney.simulate_threshold`.
    """
    module = _import_native() if prefer_native else None
    if module is not None and getattr(module, "TOURNEY_API_VERSION", None) == 1:
        native = module.simulate_threshold

        def run(outcomes: BoolArray, prices: FloatArray, p: FieldParams, seed: int) -> FloatArray:
            return np.asarray(
                native(
                    outcomes.astype(np.uint8), prices, p.n_players, p.frac_kelly, p.frac_yolo,
                    p.frac_bot, p.kelly_lo, p.kelly_hi, p.yolo_price_power, p.bot_log_mean,
                    p.bot_log_sd, seed,
                ),
                dtype=np.float64,
            )  # fmt: skip

        return NATIVE_MODULE, run

    def reference(outcomes: BoolArray, prices: FloatArray, p: FieldParams, seed: int) -> FloatArray:
        return simulate_threshold(outcomes, prices, p, seed=seed)

    return "numpy", reference


def _import_native() -> ModuleType | None:
    try:
        return importlib.import_module(NATIVE_MODULE)
    except ImportError:
        return None
