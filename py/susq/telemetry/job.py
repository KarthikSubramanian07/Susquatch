"""Hourly telemetry job → `state/research/telemetry.json` (local Competition Data, aggregates)."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

from susq.collectors.store import read_snapshots
from susq.mapping.io import load_best_map
from susq.research_env import Paths, ResearchConfig, atomic_write_text
from susq.telemetry import crowd, leaderboard
from susq.telemetry.cupdata import read_leaderboard, read_prices

CROWD_LOOKBACK = timedelta(days=14)


def tournament_members(status_path: Path) -> int | None:
    """`fieldSize` from the bridge's `state/status.json` (the tournament member count), else None."""
    try:
        value = json.loads(status_path.read_text()).get("fieldSize")
        members = int(value) if value is not None else 0
    except (OSError, ValueError, TypeError, AttributeError):
        return None
    return members if members > 0 else None


def run_telemetry(paths: Paths, cfg: ResearchConfig, *, now_ns: int) -> dict[str, Any]:
    cup = paths.data / "cup"
    lb = leaderboard.summarize(
        read_leaderboard(cup),
        lock_ns=int(cfg.lock_utc.timestamp() * 1e9),
        members=tournament_members(paths.status),
    )
    since = datetime.fromtimestamp(now_ns / 1e9, tz=UTC) - CROWD_LOOKBACK
    mm = load_best_map(paths)
    crowd_summary = (
        crowd.infer_drift(
            read_prices(cup, since=since), read_snapshots(paths.data / "reference", since=since.date()), mm
        )
        if mm is not None
        else crowd.CrowdSummary()
    )
    doc = {
        "produced_at_ns": now_ns,
        "leaderboard": lb.as_dict() if lb else None,
        "crowd": {
            "cluster_drift_pts": crowd_summary.cluster_drift_pts,
            "persistent_clusters": crowd_summary.persistent_clusters,
            "top_markets": [m.__dict__ for m in crowd_summary.markets[:25]],
        },
    }
    atomic_write_text(paths.research_state / "telemetry.json", json.dumps(doc, indent=1))
    return doc
