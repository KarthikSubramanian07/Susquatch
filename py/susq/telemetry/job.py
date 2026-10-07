"""Hourly telemetry job → `state/research/telemetry.json` (local Competition Data, aggregates)."""

from __future__ import annotations

import json
from datetime import UTC, date, datetime, timedelta
from pathlib import Path
from typing import Any

import polars as pl

from susq.collectors.store import partition_days, read_snapshots
from susq.mapping.io import load_best_map
from susq.research_env import Paths, ResearchConfig, atomic_write_text
from susq.telemetry import crowd, field, leaderboard
from susq.telemetry.cupdata import read_leaderboard, read_prices

CROWD_LOOKBACK = timedelta(days=14)
# What crowd.infer_drift needs from a reference snapshot; everything else is skipped at read time.
REFERENCE_COLUMNS = ("ts", "venue", "race_id", "party", "mid")


def tournament_members(status_path: Path) -> int | None:
    """`fieldSize` from the bridge's `state/status.json` (the tournament member count), else None."""
    try:
        value = json.loads(status_path.read_text()).get("fieldSize")
        members = int(value) if value is not None else 0
    except (OSError, ValueError, TypeError, AttributeError):
        return None
    return members if members > 0 else None


def read_reference_hourly(root: Path, *, since: date) -> pl.DataFrame:
    """Hourly mean reference mid per race and party, folded one `date=` partition at a time.

    The 14-day raw read is ~12 M rows; collecting it in one go took the scheduler past its 2 GB cap
    and the kernel killed it every 22 s from Oct 5 noon to Oct 6 night. A day collapses to a few
    thousand hourly rows, so peak memory is one day of snapshots.
    """
    hourly = []
    for day, _ in partition_days(root, since=since):
        frame = read_snapshots(root, since=day, until=day, columns=REFERENCE_COLUMNS)
        if not frame.is_empty():
            hourly.append(crowd.reference_hourly(frame))
    return pl.concat(hourly) if hourly else pl.DataFrame()


def run_telemetry(paths: Paths, cfg: ResearchConfig, *, now_ns: int) -> dict[str, Any]:
    cup = paths.data / "cup"
    lb_frame = read_leaderboard(cup)
    lb = leaderboard.summarize(
        lb_frame,
        lock_ns=int(cfg.lock_utc.timestamp() * 1e9),
        members=tournament_members(paths.status),
    )
    since = datetime.fromtimestamp(now_ns / 1e9, tz=UTC) - CROWD_LOOKBACK
    mm = load_best_map(paths)
    crowd_summary = (
        crowd.infer_drift_hourly(
            read_prices(cup, since=since),
            read_reference_hourly(paths.data / "reference", since=since.date()),
            mm,
        )
        if mm is not None
        else crowd.CrowdSummary()
    )
    # EXP-004: the cheap-leg footprint of the trade tape (aggregates per contract side, folded per day).
    footprint = field.build_footprint(field.read_tape_ladder(cup / "tape"), lb_frame, as_of_ns=now_ns)
    doc = {
        "produced_at_ns": now_ns,
        "leaderboard": lb.as_dict() if lb else None,
        "field": footprint.as_dict(),
        "crowd": {
            "cluster_drift_pts": crowd_summary.cluster_drift_pts,
            "persistent_clusters": crowd_summary.persistent_clusters,
            "top_markets": [m.__dict__ for m in crowd_summary.markets[:25]],
        },
    }
    atomic_write_text(paths.research_state / "telemetry.json", json.dumps(doc, indent=1))
    return doc
