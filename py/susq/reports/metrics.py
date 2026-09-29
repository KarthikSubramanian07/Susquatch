"""Compute the day's aggregate metrics from raw sources. Everything downstream renders these."""

from __future__ import annotations

import json
from collections import defaultdict
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, date, datetime, timedelta
from pathlib import Path
from typing import Any

import numpy as np
import polars as pl

from susq.calibrate.targets import read_current
from susq.contracts import ops_pb2
from susq.mapping.io import load_best_map
from susq.reports.sources import Fill, extract_fills, load_intents, load_journal, load_status
from susq.research_env import Paths
from susq.telemetry.cupdata import read_prices

RACER = "racer"


@dataclass
class StrategyStats:
    strategy: str
    intents: int = 0
    acked: int = 0
    rejected: int = 0
    fills: int = 0
    shares: float = 0.0
    notional: float = 0.0
    edge_vs_fair: float = 0.0  # Σ yes_exposure × (fair − yes_price), SUSQies


@dataclass
class DailyMetrics:
    day: date
    mode: str = "unknown"
    equity: float | None = None
    pnl_day: float | None = None
    pnl_total: float | None = None
    killed: bool = False
    kill_reason: str = ""
    strategies: dict[str, StrategyStats] = field(default_factory=dict)
    fills: int = 0
    racer_attempts: int = 0
    racer_hits: int = 0
    racer_latency_ms_p50: float | None = None
    racer_latency_ms_p95: float | None = None
    spread_median: float | None = None
    spread_p90: float | None = None
    tight_share: float | None = None  # share of markets quoted ≤ 2c wide
    markets_quoted: int = 0
    my_rank: int | None = None
    field_size: int | None = None
    top1_multiple: float | None = None
    t_p50: float | None = None
    t_p90: float | None = None
    t_method: str = ""
    p_first: float | None = None
    p_top3: float | None = None
    scenario: str = ""
    targets_age_h: float | None = None
    calibration_warnings: list[str] = field(default_factory=list)
    senate_gap_pts: float | None = None
    fair_flags: int = 0
    unverified_high_confidence: int = 0
    national_drift_pts: float | None = None
    risk_events: list[str] = field(default_factory=list)
    reject_codes: dict[str, int] = field(default_factory=dict)

    @property
    def racer_hit_rate(self) -> float | None:
        return self.racer_hits / self.racer_attempts if self.racer_attempts else None


def _read_json(path: Path) -> dict[str, Any] | None:
    return json.loads(path.read_text()) if path.exists() else None


def _day_bounds_ns(day: date) -> tuple[int, int]:
    start = int(datetime(day.year, day.month, day.day, tzinfo=UTC).timestamp() * 1e9)
    return start, start + 86_400 * 10**9


def _strategy_stats(
    intents: Sequence[ops_pb2.OrderIntent], fills: Sequence[Fill], fair: Mapping[str, float]
) -> dict[str, StrategyStats]:
    stats: dict[str, StrategyStats] = {}
    for i in intents:
        s = stats.setdefault(i.strategy, StrategyStats(i.strategy))
        s.intents += 1
        s.acked += i.state in (ops_pb2.INTENT_STATE_ACKED, ops_pb2.INTENT_STATE_DONE)
        s.rejected += i.state == ops_pb2.INTENT_STATE_REJECTED
    for f in fills:
        s = stats.setdefault(f.strategy, StrategyStats(f.strategy))
        s.fills += 1
        s.shares += f.quantity
        s.notional += f.quantity * f.yes_price
        if f.exchange_id in fair:
            s.edge_vs_fair += f.yes_exposure * (fair[f.exchange_id] - f.yes_price)
    return stats


def _racer(
    intents: Sequence[ops_pb2.OrderIntent], fills: Sequence[Fill], journal: Sequence[ops_pb2.JournalEntry]
) -> tuple[int, int, float | None, float | None]:
    """(attempts sent, attempts with ≥1 fill, response latency p50 ms, p95 ms)."""
    sent = {
        i.idempotency_key for i in intents if i.strategy == RACER and i.state >= ops_pb2.INTENT_STATE_SENT
    }
    hits = {f.idempotency_key for f in fills if f.idempotency_key in sent}
    lat = [e.latency_us / 1000 for e in journal if e.direction == "response" and e.idempotency_key in sent]
    p50 = float(np.percentile(lat, 50)) if lat else None
    p95 = float(np.percentile(lat, 95)) if lat else None
    return len(sent), len(hits), p50, p95


def _spreads(cup_dir: Path, day: date) -> tuple[float | None, float | None, float | None, int]:
    start = datetime(day.year, day.month, day.day, tzinfo=UTC)
    df = read_prices(cup_dir, since=start)
    if df.is_empty():
        return None, None, None, 0
    df = df.filter(pl.col("ts") < start + timedelta(days=1))
    per_market = (
        df.drop_nulls(["best_bid", "best_ask"])
        .with_columns((pl.col("best_ask") - pl.col("best_bid")).alias("spr"))
        .group_by("exchange_id")
        .agg(pl.col("spr").median())
    )
    if per_market.is_empty():
        return None, None, None, 0
    s = per_market["spr"].to_numpy()
    return float(np.median(s)), float(np.percentile(s, 90)), float(np.mean(s <= 0.02 + 1e-9)), len(s)


def collect(paths: Paths, day: date, *, now_ns: int, targets_max_age_h: float = 36.0) -> DailyMetrics:
    """Gather every metric for `day` (missing sources simply leave fields empty)."""
    m = DailyMetrics(day)
    status = load_status(paths.status)
    if status is not None:
        m.mode, m.equity, m.pnl_day, m.pnl_total = (
            status.mode,
            status.equity,
            status.pnl_day,
            status.pnl_total,
        )
        m.killed, m.kill_reason = status.killed, status.kill_reason
        m.my_rank = status.rank or None
        m.field_size = status.field_size or None
        m.risk_events += list(status.events)

    try:
        targets = read_current(paths.targets)
    except (ValueError, OSError) as exc:
        targets = None
        m.risk_events.append(f"targets unreadable: {exc}")
    fair = {f.exchange_id: f.p_yes for f in targets.fair} if targets else {}
    if targets is not None:
        t = targets.tourney
        m.p_first, m.p_top3, m.scenario = t.p_first, t.p_top3, t.scenario
        m.targets_age_h = (now_ns - targets.header.produced_at_ns) / 3.6e12

    journal = load_journal(paths.journal, day)
    all_intents = load_intents(paths.state)
    start, end = _day_bounds_ns(day)
    intents = [i for i in all_intents.values() if start <= i.created_at_ns < end]
    fills = extract_fills(journal, all_intents)
    m.fills = len(fills)
    m.strategies = _strategy_stats(intents, fills, fair)
    m.racer_attempts, m.racer_hits, m.racer_latency_ms_p50, m.racer_latency_ms_p95 = _racer(
        intents, fills, journal
    )
    m.risk_events += [f"event {e.path}: {e.body_json[:120]}" for e in journal if e.direction == "event"]
    codes: dict[str, int] = defaultdict(int)
    for i in intents:
        if i.state == ops_pb2.INTENT_STATE_REJECTED and i.last_error_code:
            codes[i.last_error_code] += 1
    m.reject_codes = dict(codes)

    m.spread_median, m.spread_p90, m.tight_share, m.markets_quoted = _spreads(paths.data / "cup", day)

    research = paths.research_state
    if (tele := _read_json(research / "telemetry.json")) is not None:
        lb = tele.get("leaderboard") or {}
        m.my_rank = m.my_rank or lb.get("my_rank")
        m.field_size = m.field_size or lb.get("field_size")
        m.top1_multiple = lb.get("top1_multiple")
        th = lb.get("threshold") or {}
        m.t_p50, m.t_p90, m.t_method = th.get("p50"), th.get("p90"), th.get("method", "")
        m.national_drift_pts = (tele.get("crowd") or {}).get("cluster_drift_pts", {}).get("national")
    if (cal := _read_json(research / "calibration.json")) is not None:
        m.calibration_warnings = list(cal.get("warnings") or [])
        senate = next((c for c in cal.get("chambers") or [] if c.get("chamber") == "senate"), None)
        m.senate_gap_pts = senate.get("gap_pts") if senate else None
    if (flags := _read_json(research / "fair_flags.json")) is not None:
        m.fair_flags = len(flags.get("flags") or [])
    if (mm := load_best_map(paths)) is not None:
        m.unverified_high_confidence = sum(
            1 for r in mm.markets if not r.verified and r.notes.startswith(("confidence=0.9", "confidence=1"))
        )
    if m.targets_age_h is not None and m.targets_age_h > targets_max_age_h:
        m.risk_events.append(f"targets are {m.targets_age_h:.0f} h old (max {targets_max_age_h:.0f} h)")
    return m
