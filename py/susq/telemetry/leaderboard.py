"""Leaderboard telemetry → field size, top-k multiples, dispersion, YOLO jumps and a fitted T.

balance ≈ 100,000 + pnl (period=all), multiple = balance / 100,000.

Fitting T (the winning multiple at resolution) from marks is only half-possible: October marks
are not resolved payoffs. So the fit is a documented blend in log space:

    log T50 = (1−w)·log prior50(N) + w·log max(top1_now, top1_now·e^{g·days_to_lock})

where g is the least-squares growth rate of log(top-1 multiple) over the last 7 days (floored at
0) and w = min(1, days observed / 14) grows as evidence accumulates. The spread (p90/p50) comes
from the SPEC prior for the observed N. Everything here is aggregate: no usernames or profile
ids leave this module.
"""

from __future__ import annotations

import math
from dataclasses import asdict, dataclass, field
from typing import Any

import numpy as np
import polars as pl

from susq.models.tourney import ThresholdFit, prior_threshold

INITIAL_BALANCE = 100_000.0
NS_PER_DAY = 86_400 * 10**9


@dataclass(frozen=True)
class JumpEvent:
    ts_ns: int
    rank: int
    multiple_before: float
    multiple_after: float


@dataclass
class LeaderboardSummary:
    as_of_ns: int
    snapshots: int
    days_observed: float
    field_size: int
    my_rank: int | None
    top1_multiple: float
    top_k_multiples: list[float]
    dispersion_log_iqr: float  # IQR of log multiples within the visible top ranks
    growth_per_day: float  # of log(top-1 multiple), last 7 days
    jumps: list[JumpEvent]
    field_size_series: list[tuple[int, int]] = field(default_factory=list)
    threshold: ThresholdFit | None = None

    def as_dict(self) -> dict[str, Any]:
        out = asdict(self)
        out["threshold"] = asdict(self.threshold) if self.threshold else None
        return out


def _growth(ts_ns: np.ndarray, top1: np.ndarray, window_days: float = 7.0) -> float:
    days = (ts_ns - ts_ns.max()) / NS_PER_DAY
    keep = days >= -window_days
    if keep.sum() < 3 or np.ptp(days[keep]) < 0.5:
        return 0.0
    slope = np.polyfit(days[keep], np.log(np.maximum(top1[keep], 1e-9)), 1)[0]
    return max(0.0, float(slope))


def detect_jumps(df: pl.DataFrame, *, ratio: float = 1.5, min_multiple: float = 1.5) -> list[JumpEvent]:
    """Per-player snapshot-to-snapshot multiple jumps (YOLO hits) — reported by rank, not identity."""
    if df.is_empty():
        return []
    seq = (
        df.sort("ts")
        .with_columns(pl.col("multiple").shift(1).over("profile_id").alias("prev"))
        .filter(pl.col("prev").is_not_null())
        .filter((pl.col("multiple") >= pl.col("prev") * ratio) & (pl.col("multiple") >= min_multiple))
    )
    return [
        JumpEvent(int(ts), int(rank), float(prev), float(m))
        for ts, rank, prev, m in zip(
            seq["ts"].dt.epoch("ns").to_list(), seq["rank"].to_list(), seq["prev"].to_list(),
            seq["multiple"].to_list(), strict=True,
        )
    ]  # fmt: skip


def fit_threshold(
    field_size: int, top1_now: float, growth_per_day: float, days_observed: float, days_to_lock: float
) -> ThresholdFit:
    prior = prior_threshold(field_size)
    w = min(1.0, max(0.0, days_observed / 14.0))
    if w == 0.0:
        return prior
    observed = max(top1_now, top1_now * math.exp(growth_per_day * max(0.0, days_to_lock)))
    log50 = (1 - w) * math.log(prior.p50) + w * math.log(max(observed, 1.0))
    p50 = math.exp(log50)
    return ThresholdFit(p50, p50 * (prior.p90 / prior.p50), field_size, f"leaderboard+prior(w={w:.2f})")


def summarize(
    df: pl.DataFrame, *, lock_ns: int, top_k: int = 10, jump_ratio: float = 1.5
) -> LeaderboardSummary | None:
    """Summarize every recorded leaderboard snapshot (None when there is no data)."""
    if df.is_empty():
        return None
    if "my_rank" not in df.columns:
        df = df.with_columns(pl.lit(None, dtype=pl.Int32).alias("my_rank"))
    df = df.with_columns(((INITIAL_BALANCE + pl.col("pnl")) / INITIAL_BALANCE).alias("multiple"))
    per_ts = (
        df.group_by("ts")
        .agg(
            pl.col("total").max().alias("total"),
            pl.col("multiple").max().alias("top1"),
            pl.col("my_rank").drop_nulls().first().alias("my_rank"),
        )
        .sort("ts")
    )
    ts_ns = per_ts["ts"].dt.epoch("ns").to_numpy()
    last_ts = per_ts["ts"].max()
    latest = df.filter(pl.col("ts") == last_ts).sort("rank")
    mults = latest["multiple"].to_numpy()
    top = sorted(mults, reverse=True)
    logs = np.log(np.maximum(mults[: max(top_k, 10) * 5], 1e-9))
    field_size = int(per_ts["total"][-1] or len(latest))
    days_observed = float((ts_ns[-1] - ts_ns[0]) / NS_PER_DAY)
    growth = _growth(ts_ns, per_ts["top1"].to_numpy())
    as_of = int(ts_ns[-1])
    my_rank = per_ts["my_rank"][-1]
    summary = LeaderboardSummary(
        as_of_ns=as_of,
        snapshots=len(per_ts),
        days_observed=days_observed,
        field_size=field_size,
        my_rank=int(my_rank) if my_rank is not None else None,
        top1_multiple=float(top[0]),
        top_k_multiples=[float(x) for x in top[:top_k]],
        dispersion_log_iqr=float(np.subtract(*np.quantile(logs, [0.75, 0.25]))) if logs.size > 1 else 0.0,
        growth_per_day=growth,
        jumps=detect_jumps(df, ratio=jump_ratio),
        field_size_series=[(int(t), int(n)) for t, n in zip(ts_ns, per_ts["total"].to_list(), strict=True)],
    )
    summary.threshold = fit_threshold(
        field_size, summary.top1_multiple, growth, days_observed, (lock_ns - as_of) / NS_PER_DAY
    )
    return summary
