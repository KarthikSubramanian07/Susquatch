"""The daily research run: calibrate → fair values → scenario book → versioned target file.

Inputs are read from disk (latest reference snapshot, market map, Cup prices, telemetry) so
the run is reproducible from its recorded input hashes. The run never places orders; rust-live
reads the resulting file and decides what to do with it.
"""

from __future__ import annotations

import json
import logging
from collections.abc import Sequence
from dataclasses import dataclass, replace
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import numpy as np

from susq.calibrate.job import CalibrationResult, run_calibration, write_outputs
from susq.calibrate.targets import InputRef, build_target_file, read_current, write_targets
from susq.collectors.quotes import RefQuote
from susq.collectors.store import partition_files
from susq.contracts import targets_pb2
from susq.fair.service import compute_fair
from susq.mapping.io import load_best_map
from susq.models.facade import get_engine, get_threshold_simulator
from susq.models.tourney import FieldParams, ThresholdFit
from susq.models.types import BoolArray, FloatArray, SimParams
from susq.optimizer.book import ROBUST_ASSUMPTIONS, OptimizerResult, build_book
from susq.optimizer.concentrate import best_concentrated, concentrated_positions
from susq.optimizer.legs import Leg, build_legs, leg_outcomes
from susq.optimizer.scenarios import scenario_grid
from susq.research_env import ConcentrateConfig, Paths, ResearchConfig, SeqCounter, sha256_bytes, sha256_file
from susq.telemetry.cupdata import read_prices, with_cup_mid
from susq.telemetry.field import FieldFootprint, leaderboard_floor, tape_field_params, tape_threshold
from susq.telemetry.rivals import blend_threshold

log = logging.getLogger(__name__)

TARGET_QUOTE_MAX_AGE_S = 15 * 60


@dataclass
class TargetsRun:
    path: Path | None
    calibration: CalibrationResult
    optimizer: OptimizerResult
    n_fair: int
    notes: str


def latest_asks(cup_dir: Path) -> dict[str, tuple[float, float | None]]:
    """exchange_id -> (best ask, depth unknown) from the newest Cup price snapshot files."""
    df = read_prices(cup_dir, newest_files=5)
    if df.is_empty():
        return {}
    last = with_cup_mid(df.sort("ts").group_by("exchange_id").last())
    return {
        e: (float(a), None)
        for e, a in zip(last["exchange_id"].to_list(), last["best_ask"].to_list(), strict=True)
        if a is not None and 0.0 < a < 1.0
    }


def _telemetry(paths: Paths) -> dict[str, Any]:
    path = paths.research_state / "telemetry.json"
    return json.loads(path.read_text()) if path.exists() else {}


def _threshold_fit(tele: dict[str, Any]) -> ThresholdFit | None:
    th = (tele.get("leaderboard") or {}).get("threshold")
    return ThresholdFit(**th) if th else None


def _footprint(tele: dict[str, Any]) -> FieldFootprint | None:
    raw = tele.get("field")
    if not raw:
        return None
    try:
        return FieldFootprint.from_dict(raw)
    except (TypeError, ValueError):
        log.exception("telemetry field footprint unreadable; using the leaderboard fit")
        return None


def select_threshold(
    tele: dict[str, Any], outcomes: BoolArray, contracts: Sequence[str], prices: FloatArray, *, seed: int
) -> tuple[FloatArray, str]:
    """T per world and a description of the method, in order of preference (EXP-004):

    1. the tape-fitted resolved field maximum, when telemetry's footprint is usable
       (`field.MIN_TRADES` trades over `field.MIN_DAYS` days with `field.MIN_CHEAP_SIDES` cheap sides),
       floored at the leaderboard anchor (`field.leaderboard_floor`);
    2. the EXP-001 leaderboard fit, sampled independently of the worlds;
    3. the SPEC field model with the α-price YOLO prior.
    """
    lb = tele.get("leaderboard") or {}
    field_size = int(lb.get("field_size") or 2000)
    fp = _footprint(tele)
    if fp is not None and fp.usable:
        floor = leaderboard_floor(lb)
        params = tape_field_params(field_size, fp)
        t = tape_threshold(outcomes, contracts, fp, params, np.random.default_rng(seed), floor=floor)
        return t, (
            f"tape(N={field_size}, bots={params.count(params.frac_bot)}, floor={floor:.2f}x; {fp.summary()})"
        )
    fit = _threshold_fit(tele)
    skipped = f"; tape not used: {fp.reason}" if fp is not None else ""
    if fit is not None:
        return fit.sample(outcomes.shape[0], seed=seed), fit.method + skipped
    name, sim_t = get_threshold_simulator()
    return sim_t(outcomes, prices, FieldParams(n_players=field_size), seed), (
        f"field-model({name}, N={field_size}){skipped}"
    )


def rival_blend(
    paths: Paths,
    threshold: FloatArray,
    outcomes: BoolArray,
    contracts: Sequence[str],
    weight: float,
    *,
    seed: int,
    now_ns: int,
) -> tuple[FloatArray, str]:
    """EXP-011 hook: T_w = (1 - w) * T + w * R, R the best modeled rival per world; no book, T unchanged.

    Only called when `[optimizer] rival_decorrelation` > 0, so the default leaves the run untouched."""
    from susq.telemetry import rivals_job

    book, why = rivals_job.load_book_file(paths.research_state / rivals_job.DEFAULT_BOOK, now_ns)
    if book is None:
        return threshold, f"rival decorrelation {weight:.2f} not applied: {why}"
    columns = {c: outcomes[:, j] for j, c in enumerate(contracts)}
    rival_t, n = rivals_job.rival_threshold_for(book, columns, outcomes.shape[0], seed=seed)
    if n == 0:
        return threshold, f"rival decorrelation {weight:.2f} not applied: no modeled rival"
    blended = blend_threshold(threshold, rival_t, weight)
    return blended, (
        f"rival decorrelation {weight:.2f} over {n} modeled rivals (R p50 {float(np.median(rival_t)):.2f}x, "
        f"T_w p50 {float(np.median(blended)):.2f}x)"
    )


def concentrate_threshold(
    paths: Paths,
    cc: ConcentrateConfig,
    tele: dict[str, Any],
    threshold: FloatArray,
    outcomes: BoolArray,
    contracts: Sequence[str],
    *,
    seed: int,
    now_ns: int,
) -> tuple[FloatArray, str]:
    """EXP-012: the per-world bar the concentrated search scores against.

    "targets" is the run's own T; "rivals" is EXP-011's best modeled rival R, floored at the leaderboard
    anchor (the leaders who trade every hour have no quiet window, so R alone would leave them out);
    "max" is the larger of the two. Without a fresh rival book, "rivals" and "max" fall back to T."""
    if cc.threshold == "targets":
        return threshold, "targets T"
    from susq.telemetry import rivals_job

    book, why = rivals_job.load_book_file(paths.research_state / rivals_job.DEFAULT_BOOK, now_ns)
    if book is None:
        return threshold, f"targets T ({cc.threshold} wanted: {why})"
    columns = {c: outcomes[:, j] for j, c in enumerate(contracts)}
    rival_t, n = rivals_job.rival_threshold_for(book, columns, outcomes.shape[0], seed=seed)
    if n == 0:
        return threshold, f"targets T ({cc.threshold} wanted: no modeled rival)"
    floor = leaderboard_floor(tele.get("leaderboard") or {})
    rival_t = np.maximum(rival_t, floor)
    desc = f"rivals R over {n} (floor {floor:.2f}x, p50 {float(np.median(rival_t)):.2f}x)"
    if cc.threshold == "rivals":
        return rival_t, desc
    return np.maximum(threshold, rival_t), f"max(targets T, {desc})"


def apply_concentrate(
    result: OptimizerResult,
    legs: Sequence[Leg],
    outcomes: BoolArray,
    threshold: FloatArray,
    cc: ConcentrateConfig,
    *,
    equity: float,
    how: str,
) -> tuple[OptimizerResult, str, str | None]:
    """EXP-012: (result, note, scenario override). "report" only adds a note; "commit" swaps the book in
    when its P(1st) beats the scenario book's by `min_gain`."""
    if cc.mode == "off":
        return result, "", None
    book = best_concentrated(legs, outcomes, threshold, cc, equity=equity)
    if book is None:
        return result, f"; EXP-012 ({cc.mode}): no leg at or under ask {cc.max_ask}", None
    note = f"; EXP-012 ({cc.mode}, vs {how}): {book.describe(legs)}"
    scenario_pf = result.p_first if result.positions else 0.0
    if cc.mode != "commit" or book.p_first < scenario_pf + cc.min_gain:
        if cc.mode == "commit":
            note += f"; kept the scenario book (P(1st) {scenario_pf:.4f}, gain under {cc.min_gain})"
        return result, note, None
    committed = replace(
        result,
        positions=concentrated_positions(book, legs, cc),
        p_first=book.p_first,
        p_top3=min(1.0, 2.5 * book.p_first),
        book_cost=book.cost,
    )
    scenario = "concentrated:" + "+".join(legs[j].exchange_id for j in book.legs)
    return committed, note + f"; committed in place of the scenario book (P(1st) {scenario_pf:.4f})", scenario


def robust_thresholds(worlds: int) -> dict[str, FloatArray]:
    """The constant field assumptions every ranking is checked against (thin, medium, tail)."""
    return {name: np.full(worlds, t) for name, t in ROBUST_ASSUMPTIONS.items()}


def incumbent_scenario(targets_dir: Path) -> str | None:
    """The scenario the current targets file committed (EXP-009); None if absent or unreadable."""
    try:
        tf = read_current(targets_dir)
    except Exception:
        log.warning("current targets file unreadable; no incumbent scenario", exc_info=True)
        return None
    return (tf.tourney.scenario or None) if tf is not None else None


def incumbent_legs(targets_dir: Path) -> dict[str, str]:
    """Exchange id -> tranche of the current targets file's legs (EXP-009 leg stickiness); {} if absent."""
    try:
        tf = read_current(targets_dir)
    except Exception:
        log.warning("current targets file unreadable; no incumbent legs", exc_info=True)
        return {}
    return {p.exchange_id: p.tranche for p in tf.positions} if tf is not None else {}


HELD_MAX_AGE_S = 6 * 3600


def held_long(status_path: Path, now_ns: int, max_age_s: float = HELD_MAX_AGE_S) -> set[str]:
    """Exchange ids the live account holds YES shares in, from the trader's `state/status.json` (EXP-010).

    Empty when the file is missing, unreadable, not live, or older than `max_age_s`."""
    try:
        doc = json.loads(status_path.read_text())
        produced = int((doc.get("header") or {}).get("producedAtNs") or 0)
    except (OSError, ValueError, TypeError, AttributeError):
        return set()
    if doc.get("mode") != "live" or now_ns - produced > max_age_s * 1e9:
        return set()
    out: set[str] = set()
    for pos in doc.get("positions") or []:
        try:
            if float(pos.get("quantity") or 0) > 0:
                out.add(str(pos["exchangeId"]))
        except (KeyError, TypeError, ValueError):
            continue
    return out


def _input_refs(paths: Paths, now_ns: int) -> list[InputRef]:
    refs: list[InputRef] = []
    ref_files = partition_files(paths.data / "reference")
    if ref_files:
        refs.append(
            InputRef("reference_mids", sha256_file(ref_files[-1]), int(ref_files[-1].stat().st_mtime_ns))
        )
    for name, path in (
        ("market_map", paths.market_map if paths.market_map.exists() else paths.market_map_draft),
        ("telemetry", paths.research_state / "telemetry.json"),
    ):
        if path.exists():
            refs.append(InputRef(name, sha256_file(path), int(path.stat().st_mtime_ns)))
    cup = sorted((paths.data / "cup" / "prices").rglob("*.parquet"), key=lambda f: f.stat().st_mtime)
    if cup:
        refs.append(InputRef("cup_prices", sha256_file(cup[-1]), int(cup[-1].stat().st_mtime_ns)))
    refs.append(InputRef("sim_params", sha256_bytes(repr(SimParams()).encode()), now_ns))
    return refs


def run_targets(
    paths: Paths,
    cfg: ResearchConfig,
    quotes: list[RefQuote],
    *,
    now_ns: int,
    n_draws: int = 100_000,
    seed: int = 20261103,
    equity: float = 100_000.0,
    require_verified: bool = True,
    write: bool = True,
) -> TargetsRun:
    mm = load_best_map(paths)
    engine = get_engine()
    calib = run_calibration(quotes, engine, market_map=mm, n_draws=n_draws, seed=seed, now_ns=now_ns)
    write_outputs(calib, paths.research_state, produced_at_ns=now_ns)

    rules = list(mm.markets) if mm else []
    fair_cfg = replace(cfg.fair, quote_max_age_s=max(cfg.fair.quote_max_age_s, TARGET_QUOTE_MAX_AGE_S))
    fair, _ = compute_fair(rules, quotes, calib.sim_probs, fair_cfg, now_ns=now_ns)

    asks = latest_asks(paths.data / "cup")
    universe = build_legs(rules, fair, asks, require_verified=False)
    all_outcomes, universe = leg_outcomes(calib.draws, calib.specs, universe)
    tradable = [i for i, leg in enumerate(universe) if leg.verified or not require_verified]
    legs = [universe[i] for i in tradable]
    outcomes = all_outcomes[:, tradable]

    tele = _telemetry(paths)
    field_size = int((tele.get("leaderboard") or {}).get("field_size") or 2000)
    threshold, t_source = select_threshold(
        tele,
        all_outcomes,
        [leg.exchange_id for leg in universe],
        np.array([leg.ask for leg in universe]),
        seed=seed,
    )
    rival_note = ""
    if cfg.optimizer.rival_decorrelation > 0.0:  # EXP-011, off by default: nothing below changes at 0.0
        threshold, rival_note = rival_blend(
            paths, threshold, all_outcomes, [leg.exchange_id for leg in universe],
            cfg.optimizer.rival_decorrelation, seed=seed, now_ns=now_ns,
        )  # fmt: skip
        t_source += f"; {rival_note}"
    drift = (tele.get("crowd") or {}).get("cluster_drift_pts") or {}
    now = datetime.fromtimestamp(now_ns / 1e9, tz=UTC)
    book_frac = cfg.optimizer.book_frac_at(now)
    early = now < cfg.optimizer.early_until_utc
    result = build_book(
        legs,
        outcomes,
        scenario_grid(calib.draws),
        threshold,
        cfg.optimizer,
        equity=equity,
        cluster_drift_pts=drift,
        book_frac=book_frac,
        robust_thresholds=robust_thresholds(threshold.shape[0]),
        incumbent=incumbent_scenario(paths.targets),
        incumbent_legs=incumbent_legs(paths.targets),
        held_long=held_long(paths.status, now_ns),
    )
    conc_note, conc_scenario = "", None
    if cfg.concentrate.mode != "off":  # EXP-012, off by default: nothing below changes when off
        conc_t, how = concentrate_threshold(
            paths, cfg.concentrate, tele, threshold, all_outcomes, [leg.exchange_id for leg in universe],
            seed=seed, now_ns=now_ns,
        )  # fmt: skip
        # T is per world, so it applies unchanged to the tradable columns
        result, conc_note, conc_scenario = apply_concentrate(
            result, legs, outcomes, conc_t, cfg.concentrate, equity=equity, how=how
        )
    tourney = targets_pb2.TourneyEstimate(
        p_first=result.p_first,
        p_top3=result.p_top3,
        field_size=field_size,
        threshold_t_p50=float(np.median(threshold)),
        threshold_t_p90=float(np.quantile(threshold, 0.9)),
        scenario=conc_scenario
        or (result.best.scenario.scenario_id if result.best and result.positions else ""),
    )
    sizing = (
        f"early book frac {book_frac:.2f} until {cfg.optimizer.early_until_utc:%Y-%m-%dT%H:%MZ}"
        if early
        else f"full book frac {book_frac:.2f}"
    )
    notes = (
        f"engine={calib.engine} draws={calib.n_draws}; T from {t_source}; legs {len(legs)}/{len(universe)} "
        f"tradable (require_verified={require_verified}); {sizing}; {result.explanation}{conc_note}"
        + (f"; WARN: {' | '.join(calib.warnings)}" if calib.warnings else "")
        + ("; p_top3 = 2.5 x P(1st) heuristic (SPEC 6)" if result.positions else "")
    )
    path = None
    if write:
        tf = build_target_file(
            tournament_slug=cfg.tournament_slug,
            seq=SeqCounter(paths.research_state / "seq-targets.txt").next(),
            now_ns=now_ns,
            valid_for_s=cfg.targets_max_age_h * 3600,
            inputs=_input_refs(paths, now_ns),
            fair=fair,
            positions=result.positions,
            cluster_limits=cfg.cluster_limits,
            tourney=tourney,
            notes=notes,
        )
        path = write_targets(
            paths.targets, tf, pointer_seq=SeqCounter(paths.research_state / "seq-pointer.txt").next()
        )
        log.info("wrote %s (%d fair, %d positions)", path.name, len(fair), len(result.positions))
    return TargetsRun(path, calib, result, len(fair), notes)
