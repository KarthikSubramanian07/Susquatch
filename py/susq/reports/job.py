"""Daily report + review packet for one UTC day."""

from __future__ import annotations

from datetime import date
from pathlib import Path

from susq.reports.daily import write_daily
from susq.reports.metrics import collect
from susq.reports.packets import write_packet
from susq.research_env import Paths, ResearchConfig
from susq.telemetry.cupdata import read_leaderboard


def run_report(paths: Paths, cfg: ResearchConfig, day: date, *, now_ns: int) -> tuple[Path, Path]:
    metrics = collect(
        paths,
        day,
        now_ns=now_ns,
        targets_max_age_h=cfg.targets_max_age_h,
        cluster_limits=cfg.cluster_limits,
    )
    daily = write_daily(paths.reports, metrics)
    lb = read_leaderboard(paths.data / "cup")
    usernames = set(lb["username"].drop_nulls().to_list()) if "username" in lb.columns else set()
    packet = write_packet(paths.reports, metrics, usernames=usernames)
    return daily, packet
