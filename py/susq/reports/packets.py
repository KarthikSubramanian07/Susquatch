"""Review packets for the private reports repo: aggregates only, then scrubbed.

`reports/packets/<date>/` holds `summary.md`, `metrics.csv` (key,value) and `strategies.csv`.
Packets are built from numbers only (counts, sums, rates, quantiles): no market ids, order ids,
exchange ids, usernames, profile ids, idempotency keys or free-text journal bodies. After writing,
`scrub()` re-reads every file and refuses (deleting the packet) if anything that looks like raw
Competition Data slipped through.
"""

from __future__ import annotations

import csv
import io
import re
import shutil
from collections.abc import Iterable
from pathlib import Path

from susq.reports.metrics import DailyMetrics
from susq.reports.rules import propose
from susq.research_env import atomic_write_text

# Field names that only appear in raw platform payloads.
FORBIDDEN_FIELDS = (
    "orderId", "order_id", "profileId", "profile_id", "username", "idempotencyKey", "idempotency_key",
    "exchangeId", "exchange_id", "marketId", "market_id", "tournamentId", "tournament_id", "body_json",
    "requestId", "request_id", "correlation_id", "intent_id",
)  # fmt: skip
_UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", re.IGNORECASE)
_LONG_ID = re.compile(r"\b\d{9,}\b")  # platform order ids / token ids; aggregates never need 9+ digits


class PacketLeakError(RuntimeError):
    """A packet contained something that is not an aggregate."""


def scrub_text(text: str, *, usernames: Iterable[str] = ()) -> list[str]:
    """Problems found in `text` (empty list = clean)."""
    problems = [f"forbidden field name {f!r}" for f in FORBIDDEN_FIELDS if f in text]
    problems += [f"uuid {u}" for u in _UUID.findall(text)]
    problems += [f"long numeric id {n}" for n in _LONG_ID.findall(text)]
    for name in usernames:
        if name and len(name) >= 3 and re.search(rf"\b{re.escape(name)}\b", text):
            problems.append("a leaderboard username")
    return problems


def scrub(packet_dir: Path, *, usernames: Iterable[str] = ()) -> None:
    """Raise PacketLeakError if any file in the packet fails `scrub_text`."""
    names = list(usernames)
    problems = []
    for path in sorted(packet_dir.iterdir()):
        problems += [f"{path.name}: {p}" for p in scrub_text(path.read_text(), usernames=names)]
    if problems:
        raise PacketLeakError("; ".join(problems))


def _csv(rows: list[list[object]]) -> str:
    buf = io.StringIO()
    csv.writer(buf, lineterminator="\n").writerows(rows)
    return buf.getvalue()


def _num(value: float | int | None) -> str:
    if value is None:
        return ""
    return f"{value:.6g}" if isinstance(value, float) else str(value)


def packet_metrics(m: DailyMetrics) -> list[list[object]]:
    """(key, value) rows. Only numbers and fixed enums."""
    return [
        ["key", "value"],
        ["date", m.day.isoformat()],
        ["mode", m.mode if m.mode in {"paper", "live"} else "unknown"],
        ["equity", _num(m.equity)],
        ["pnl_day", _num(m.pnl_day)],
        ["pnl_total", _num(m.pnl_total)],
        ["killed", int(m.killed)],
        ["fills", m.fills],
        ["racer_attempts", m.racer_attempts],
        ["racer_hits", m.racer_hits],
        ["racer_latency_ms_p50", _num(m.racer_latency_ms_p50)],
        ["racer_latency_ms_p95", _num(m.racer_latency_ms_p95)],
        ["spread_median", _num(m.spread_median)],
        ["spread_p90", _num(m.spread_p90)],
        ["tight_share", _num(m.tight_share)],
        ["markets_quoted", m.markets_quoted],
        ["my_rank", _num(m.my_rank)],
        ["field_size", _num(m.field_size)],
        ["top1_multiple", _num(m.top1_multiple)],
        ["t_p50", _num(m.t_p50)],
        ["t_p90", _num(m.t_p90)],
        ["p_first", _num(m.p_first)],
        ["p_top3", _num(m.p_top3)],
        ["targets_age_h", _num(m.targets_age_h)],
        ["senate_gap_pts", _num(m.senate_gap_pts)],
        ["national_drift_pts", _num(m.national_drift_pts)],
        ["fair_flags", m.fair_flags],
        ["risk_events", len(m.risk_events) + len(m.calibration_warnings)],
        ["rejects", sum(m.reject_codes.values())],
        ["cash", _num(m.cash)],
        ["cash_frac", _num(m.cash_frac)],
        ["total_gross_frac", _num(m.total_gross_frac)],
        *_cluster_rows(m),
        ["scenario_orders", m.scenario_orders],
        ["scenario_refused_caps", m.scenario_refused_caps],
    ]


def _cluster_rows(m: DailyMetrics) -> list[list[object]]:
    """`cluster_<n>` (label) and `cluster_<n>_cap_use` (used / cap) for the three closest to their caps."""
    rows: list[list[object]] = []
    for n in range(1, 4):
        name, frac = m.cluster_use[n - 1] if n <= len(m.cluster_use) else ("", None)
        rows += [[f"cluster_{n}", name], [f"cluster_{n}_cap_use", _num(frac)]]
    return rows


def packet_summary(m: DailyMetrics) -> str:
    lines = [f"# Susquatch review packet: {m.day.isoformat()}", "", "Aggregates only. Proposed changes:", ""]
    lines += [f"{i}. [{p.rule_id}] {p.text}" for i, p in enumerate(propose(m), 1)] or ["No changes proposed."]
    lines += ["", "See metrics.csv and strategies.csv."]
    return "\n".join(lines) + "\n"


def write_packet(reports_dir: Path, m: DailyMetrics, *, usernames: Iterable[str] = ()) -> Path:
    """Write and scrub the packet; on a leak the directory is removed and the error re-raised."""
    out = reports_dir / "packets" / m.day.isoformat()
    tmp = out.with_name(f".{out.name}.tmp")
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True)
    atomic_write_text(tmp / "summary.md", packet_summary(m))
    atomic_write_text(tmp / "metrics.csv", _csv(packet_metrics(m)))
    strategies: list[list[object]] = [
        ["strategy", "intents", "acked", "rejected", "fills", "shares", "notional", "edge_vs_fair"]
    ]
    strategies += [
        [
            s.strategy,
            s.intents,
            s.acked,
            s.rejected,
            s.fills,
            _num(s.shares),
            _num(s.notional),
            _num(s.edge_vs_fair),
        ]
        for s in sorted(m.strategies.values(), key=lambda s: s.strategy)
    ]
    atomic_write_text(tmp / "strategies.csv", _csv(strategies))
    try:
        scrub(tmp, usernames=usernames)
    except PacketLeakError:
        shutil.rmtree(tmp, ignore_errors=True)
        raise
    shutil.rmtree(out, ignore_errors=True)
    tmp.rename(out)
    return out
