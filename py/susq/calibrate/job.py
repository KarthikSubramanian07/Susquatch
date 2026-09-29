"""Daily calibration: fit the simulator's per-race μ to real money, then audit the chambers.

1. Build the race set: every race in the market map, every 2026 Senate seat (needed to count the
   chamber) and the House-control pseudo-race.
2. Targets = two-way real-money consensus for the left side. Unpriced Senate seats default to the
   incumbent party at 97%; other unpriced races are left out of the simulation.
3. Draw shocks once (μ = 0), bisect μ per race, apply.
4. Compare the Senate-control probability *implied by the race outcomes* with the Senate-control
   market. SPEC §5 wants it within 2-4 points; a bigger gap means "assume a bug before assuming
   edge" and is reported as a warning, never traded on. House control is calibrated directly, so
   its gap is zero by construction (see models/chamber.py).
"""

from __future__ import annotations

import json
import logging
from collections.abc import Sequence
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from typing import Any

import numpy as np

from susq.calibrate.consensus import Consensus, consensus, left_party, left_target
from susq.collectors.quotes import RefQuote
from susq.contracts import ops_pb2
from susq.mapping.geo import NAMED_CLUSTERS, SENATE_SEATS_UP, region_of
from susq.mapping.races import DEM, IND, REP, RaceKey, race_from_id
from susq.models.calibration import apply_mu, calibrate
from susq.models.chamber import (
    HOUSE_CONTROL_ID,
    SENATE_CONTROL_ID,
    house_control_spec,
    house_dem_control,
    senate_dem_control,
    senate_dem_seats,
    senate_seat_specs,
)
from susq.models.types import RaceSpec, SimDraws, SimEngine, SimParams
from susq.research_env import atomic_write_text

log = logging.getLogger(__name__)

UNPRICED_SAFE_P = 0.97
GAP_OK_PTS = 4.0
IND_RACE_SD = 4.0  # SPEC §5: larger ε with independents


@dataclass
class ChamberCheck:
    chamber: str
    implied: float
    market: float | None
    gap_pts: float | None
    calibrated_directly: bool


@dataclass
class CalibrationResult:
    specs: list[RaceSpec]
    draws: SimDraws
    engine: str
    n_draws: int
    seed: int
    targets: dict[str, float]
    sim_probs: dict[str, dict[str, float]]  # race_id -> party -> P(win)
    chambers: list[ChamberCheck]
    unpriced: list[str]
    warnings: list[str] = field(default_factory=list)

    def summary(self) -> dict[str, Any]:
        return {
            "engine": self.engine,
            "n_draws": self.n_draws,
            "seed": self.seed,
            "races": len(self.specs),
            "unpriced": self.unpriced,
            "chambers": [asdict(c) for c in self.chambers],
            "warnings": self.warnings,
        }


def _spec_for(key: RaceKey, left: str) -> RaceSpec:
    named = [name for name, states in NAMED_CLUSTERS.items() if key.state in states]
    if key.office == "HOUSE":
        named.append("house")
    race_sd = IND_RACE_SD if left == IND else 3.0
    return RaceSpec(
        key.race_id, key.state, region_of(key.state), tuple(named), race_sd=race_sd, left_party=left
    )


def race_universe(
    market_map: ops_pb2.MarketMap | None, cons: dict[tuple[str, str], Consensus]
) -> tuple[list[RaceSpec], dict[str, float], list[str]]:
    """Specs, left-side targets and the list of unpriced races kept at a default."""
    race_ids: set[str] = set()
    if market_map is not None:
        race_ids |= {r.race_id for r in market_map.markets if r.kind == "race_party" and r.race_id}
    else:  # no Cup map yet: statewide races only (House districts would dominate the draw matrix)
        race_ids |= {rid for rid, _ in cons if rid.split("-")[1] in {"SEN", "GOV"}}
    specs: list[RaceSpec] = []
    targets: dict[str, float] = {}
    unpriced: list[str] = []
    for rid in sorted(race_ids):
        try:
            key = race_from_id(rid)
        except ValueError:
            log.warning("skipping unknown race id %s", rid)
            continue
        target = left_target(rid, cons)
        if target is None:
            unpriced.append(rid)
            continue
        specs.append(_spec_for(key, left_party(rid, cons)))
        targets[rid] = target
    for seat in senate_seat_specs(specs):
        target = left_target(seat.race_id, cons)
        if target is None:
            target = UNPRICED_SAFE_P if SENATE_SEATS_UP[seat.state] == DEM else 1 - UNPRICED_SAFE_P
            unpriced.append(seat.race_id)
        specs.append(_spec_for(race_from_id(seat.race_id), left_party(seat.race_id, cons)))
        targets[seat.race_id] = target
    specs.append(house_control_spec())
    if (house := cons.get((HOUSE_CONTROL_ID, DEM))) is not None:
        targets[HOUSE_CONTROL_ID] = house.p
    elif (house_r := cons.get((HOUSE_CONTROL_ID, REP))) is not None:
        targets[HOUSE_CONTROL_ID] = 1 - house_r.p
    else:
        unpriced.append(HOUSE_CONTROL_ID)
        targets[HOUSE_CONTROL_ID] = 0.5
    return specs, targets, sorted(set(unpriced))


def run_calibration(
    quotes: Sequence[RefQuote],
    engine: SimEngine,
    *,
    market_map: ops_pb2.MarketMap | None = None,
    params: SimParams | None = None,
    n_draws: int = 100_000,
    seed: int = 20261103,
    now_ns: int | None = None,
    max_quote_age_s: float = 6 * 3600,
) -> CalibrationResult:
    params = params or SimParams()
    cons = consensus(quotes, now_ns=now_ns, max_age_s=max_quote_age_s)
    specs, targets, unpriced = race_universe(market_map, cons)
    shocks = engine.simulate(specs, params, n_draws, seed)
    mu = calibrate(shocks, targets)
    fitted = apply_mu(shocks, mu)
    specs = [replace(s, mu=mu[s.race_id]) for s in specs]

    left_p = fitted.left_wins().mean(axis=0)
    sim_probs: dict[str, dict[str, float]] = {}
    for i, s in enumerate(specs):
        if s.race_id == HOUSE_CONTROL_ID:
            continue
        sim_probs[s.race_id] = {p: s.win_party_prob(p, float(left_p[i])) for p in (s.left_party, REP)}
    senate_d = float(senate_dem_control(fitted, specs, seed=seed).mean())
    house_d = float(house_dem_control(fitted).mean())
    sim_probs[SENATE_CONTROL_ID] = {DEM: senate_d, REP: 1 - senate_d}
    sim_probs[HOUSE_CONTROL_ID] = {DEM: house_d, REP: 1 - house_d}

    chambers = [
        _chamber_check("senate", senate_d, cons.get((SENATE_CONTROL_ID, DEM)), direct=False),
        _chamber_check("house", house_d, cons.get((HOUSE_CONTROL_ID, DEM)), direct=True),
    ]
    warnings = [
        f"{c.chamber} implied {c.implied:.3f} vs market {c.market:.3f} ({c.gap_pts:+.1f} pts) exceeds "
        f"{GAP_OK_PTS:.0f} pts: assume a bug before assuming edge"
        for c in chambers
        if c.gap_pts is not None and c.market is not None and abs(c.gap_pts) > GAP_OK_PTS
    ]
    seats = senate_dem_seats(fitted, specs, seed=seed)
    log.info(
        "calibrated %d races on %s; senate D implied %.3f (median seats %.0f), house D %.3f",
        len(specs), engine.name, senate_d, float(np.median(seats)), house_d,
    )  # fmt: skip
    return CalibrationResult(
        specs, fitted, engine.name, n_draws, seed, targets, sim_probs, chambers, unpriced, warnings
    )


def _chamber_check(name: str, implied: float, market: Consensus | None, *, direct: bool) -> ChamberCheck:
    if market is None:
        return ChamberCheck(name, implied, None, None, direct)
    return ChamberCheck(name, implied, market.p, (implied - market.p) * 100, direct)


def write_outputs(
    result: CalibrationResult, research_state: Path, *, produced_at_ns: int
) -> tuple[Path, Path]:
    """`sim_probs.json` (read by the fair service) and `calibration.json` (read by reports)."""
    probs_path = research_state / "sim_probs.json"
    summary_path = research_state / "calibration.json"
    atomic_write_text(
        probs_path,
        json.dumps(
            {"produced_at_ns": produced_at_ns, "engine": result.engine, "probs": result.sim_probs}, indent=1
        ),
    )
    atomic_write_text(
        summary_path, json.dumps({"produced_at_ns": produced_at_ns, **result.summary()}, indent=1)
    )
    return probs_path, summary_path


def load_sim_probs(research_state: Path) -> tuple[dict[str, dict[str, float]], int]:
    """(probs, produced_at_ns); empty when calibration has not run yet."""
    path = research_state / "sim_probs.json"
    if not path.exists():
        return {}, 0
    doc = json.loads(path.read_text())
    return {k: {p: float(x) for p, x in v.items()} for k, v in doc["probs"].items()}, int(
        doc["produced_at_ns"]
    )
