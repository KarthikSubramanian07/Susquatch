"""`susq research rivals`: the endgame rival model run (EXP-011), and the reader the optimizer hook uses.

Reads only: the leaderboard and price recordings under `data/cup`, the market map, the latest reference
quotes (to calibrate the same simulator the targets run uses), `state/research/telemetry.json` (the
fitted T), `state/status.json` (our book) and the current targets file (our scenario). Writes:

* `reports/rivals/rivals-<UTC stamp>.json` and `.md`: aggregates only, rivals as slots;
* optionally the slot books (exchange ids and shares, no accounts) to `--book-out`, which the
  `[optimizer] rival_decorrelation` hook reads. Both locations are gitignored and stay on the box.

Never places, sizes or cancels an order.
"""

from __future__ import annotations

import json
import logging
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

import numpy as np

from susq.calibrate.job import run_calibration
from susq.calibrate.targets import read_current
from susq.collectors.quotes import RefQuote
from susq.mapping.io import load_best_map
from susq.models.facade import get_engine
from susq.models.types import FloatArray
from susq.optimizer.scenarios import scenario_grid
from susq.research_env import Paths, ResearchConfig, atomic_write_text
from susq.telemetry import rivals
from susq.telemetry.cupdata import read_leaderboard

log = logging.getLogger(__name__)

BOOK_MAX_AGE_H = 36.0
DEFAULT_BOOK = "rivals_book.json"  # under state/research


def our_book(
    status_path: Path, targets_dir: Path, marks: Mapping[str, float], fallback_balance: float
) -> tuple[float, dict[str, float], dict[str, float], str]:
    """(balance, signed shares, marks, source) of our own account.

    The bridge's live `state/status.json` first (equity and signed positions with their marks); else the
    current targets file's positions as if filled, at the panel marks, on `fallback_balance`."""
    try:
        doc = json.loads(status_path.read_text())
        if doc.get("mode") == "live" and doc.get("equity"):
            pos: dict[str, float] = {}
            own_marks: dict[str, float] = {}
            for p in doc.get("positions") or []:
                q = float(p.get("quantity") or 0.0)
                if q == 0:
                    continue
                ex = str(p["exchangeId"])
                pos[ex] = q
                m = float(p.get("mark") or 0.0)
                own_marks[ex] = m if 0.0 < m < 1.0 else float(marks.get(ex, 0.5))
            return float(doc["equity"]), pos, own_marks, "status"
    except (OSError, ValueError, TypeError, KeyError):
        pass
    try:
        tf = read_current(targets_dir)
    except Exception:
        tf = None
    if tf is None:
        return fallback_balance, {}, {}, "none"
    pos = {p.exchange_id: float(p.target_qty) for p in tf.positions}
    return fallback_balance, pos, {ex: float(marks.get(ex, 0.5)) for ex in pos}, "targets"


@dataclass
class RivalsRun:
    report: rivals.RivalReport
    json_path: Path | None
    md_path: Path | None
    book_path: Path | None


def run_rivals(
    paths: Paths,
    cfg: ResearchConfig,
    quotes: Sequence[RefQuote],
    *,
    now_ns: int,
    focus_n: int = 5,
    wide_n: int = 20,
    n_draws: int = 50_000,
    seed: int = 20261103,
    source: str = "mid",
    lag_min: float = 0.0,
    fit: rivals.FitConfig | None = None,
    out_dir: Path | None = None,
    book_out: Path | None = None,
) -> RivalsRun:
    from susq.calibrate.pipeline import select_threshold  # the targets run's own T (lazy: no import cycle)

    fit = fit or rivals.FitConfig()
    cup = paths.data / "cup"
    lb = read_leaderboard(cup)
    now = datetime.fromtimestamp(now_ns / 1e9, tz=UTC)
    snaps = rivals.snapshot_times(lb, since=now - timedelta(hours=fit.lookback_h + 2))
    panel = rivals.read_price_panel(cup / "prices", snaps, source=source, lag=timedelta(minutes=lag_min))
    marks = panel.latest()
    ids = rivals.select_rivals(lb, max(focus_n, wide_n))
    books = rivals.infer_books(lb, panel, ids, fit)
    del ids  # identities end here: everything below is slots

    mm = load_best_map(paths)
    rules = list(mm.markets) if mm else []
    titles = {r.exchange_id: r.title for r in rules}
    calib = run_calibration(
        list(quotes), get_engine(), market_map=mm, n_draws=n_draws, seed=seed, now_ns=now_ns
    )
    columns = rivals.outcome_columns(calib.draws, calib.specs, rules)
    outcomes = rivals.Outcomes(columns, calib.draws.n, seed=seed)

    contracts = sorted(columns)
    matrix = (
        np.column_stack([columns[c] for c in contracts])
        if contracts
        else np.zeros((calib.draws.n, 0), dtype=bool)
    )
    tele_path = paths.research_state / "telemetry.json"
    tele = json.loads(tele_path.read_text()) if tele_path.exists() else {}
    field_t, t_source = select_threshold(
        tele, matrix, contracts, np.array([marks.get(c, 0.5) for c in contracts]), seed=seed
    )

    mine = rivals.our_rank(lb)
    latest = lb.filter(lb["ts"] == lb["ts"].max()) if not lb.is_empty() else lb
    my_pnl = (
        latest.filter(latest["rank"] == mine)["pnl"] if mine is not None and not latest.is_empty() else None
    )
    fallback = rivals.INITIAL_BALANCE + (float(my_pnl[0]) if my_pnl is not None and my_pnl.len() else 0.0)
    balance, our_pos, our_marks, our_source = our_book(paths.status, paths.targets, marks, fallback)
    ours = rivals.payout_multiple(balance, our_pos, {**marks, **our_marks}, outcomes)

    scenario, mask = "", None
    try:
        tf = read_current(paths.targets)
        scenario = tf.tourney.scenario if tf is not None else ""
    except Exception:
        log.warning("current targets file unreadable; no scenario", exc_info=True)
    if scenario:
        mask = next((s.mask for s in scenario_grid(calib.draws) if s.scenario_id == scenario), None)

    report = rivals.analyze(
        books, marks, outcomes, ours, our_pos, field_t,
        our_scenario=scenario, scenario_mask=mask, our_mark_multiple=balance / rivals.INITIAL_BALANCE,
        titles=titles, focus_n=focus_n, produced_at_ns=now_ns,
    )  # fmt: skip
    report.notes += [
        f"valuation {source} lag {lag_min:g} min over {len(snaps)} snapshots, "
        f"{len(panel.exchanges)} exchanges",
        f"fitted T from {t_source}",
        f"our book from {our_source} ({len(our_pos)} positions)",
        f"engine {calib.engine}, {calib.n_draws} draws",
    ]
    out_dir = out_dir or paths.reports / "rivals"
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    out_dir.mkdir(parents=True, exist_ok=True)
    json_path, md_path = out_dir / f"rivals-{stamp}.json", out_dir / f"rivals-{stamp}.md"
    atomic_write_text(json_path, json.dumps(report.as_dict(), indent=1))
    atomic_write_text(md_path, rivals.render_markdown(report))
    book_path = None
    if book_out is not None:
        book_out.parent.mkdir(parents=True, exist_ok=True)
        atomic_write_text(book_out, json.dumps(rivals.RivalBookFile(now_ns, marks, books).as_dict()))
        book_path = book_out
    return RivalsRun(report, json_path, md_path, book_path)


# ── the optimizer hook's input ─────────────────────────────────────────────────────────────


def load_book_file(
    path: Path, now_ns: int, max_age_h: float = BOOK_MAX_AGE_H
) -> tuple[rivals.RivalBookFile | None, str]:
    """(book file, why not) — None when missing, unreadable or older than `max_age_h`."""
    try:
        book = rivals.RivalBookFile.from_dict(json.loads(path.read_text()))
    except FileNotFoundError:
        return None, "no rival book"
    except (OSError, ValueError, TypeError, KeyError):
        return None, "rival book unreadable"
    age_h = (now_ns - book.produced_at_ns) / rivals.NS_PER_HOUR
    if age_h > max_age_h:
        return None, f"rival book {age_h:.0f} h old > {max_age_h:.0f} h"
    return book, ""


def rival_threshold_for(
    book: rivals.RivalBookFile, columns: Mapping[str, np.ndarray], worlds: int, *, seed: int
) -> tuple[FloatArray, int]:
    """(R per world, modeled rivals) on the targets run's own worlds (`columns`: exchange -> YES)."""
    outcomes = rivals.Outcomes(columns, worlds, seed=seed)
    matrix = rivals.rival_matrix(book.books, book.marks, outcomes)
    return rivals.rival_threshold(matrix), int(matrix.shape[1])
