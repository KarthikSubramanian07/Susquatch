"""Render `reports/daily/<date>.md` (local, Competition Data) from `DailyMetrics`."""

from __future__ import annotations

from pathlib import Path

from susq.reports.metrics import DailyMetrics
from susq.reports.rules import propose
from susq.research_env import atomic_write_text


def _f(value: float | None, fmt: str = "{:.2f}", missing: str = "n/a") -> str:
    return missing if value is None else fmt.format(value)


def render(m: DailyMetrics) -> str:
    proposals = propose(m)
    lines = [
        f"# Susquatch daily report: {m.day.isoformat()}",
        "",
        f"Mode **{m.mode}**; equity {_f(m.equity, '{:,.0f}')}; P&L today {_f(m.pnl_day, '{:+,.0f}')}; "
        f"total {_f(m.pnl_total, '{:+,.0f}')}." + (f" **KILLED:** {m.kill_reason}" if m.killed else ""),
        "",
        "## Top 3 proposed changes",
        "",
    ]
    lines += [f"{i}. {p.text} `[{p.rule_id}]`" for i, p in enumerate(proposals, 1)] or [
        "No changes proposed. Keep running."
    ]
    lines += [
        "",
        "## P&L by strategy",
        "",
        "Edge is the mark-to-fair value of today's fills, Σ YES exposure × (fair − fill price), "
        "using the fair values in the current target file.",
        "",
        "| Strategy | Intents | Acked | Rejected | Fills | Shares | Notional | Edge vs fair |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for s in sorted(m.strategies.values(), key=lambda s: s.strategy):
        lines.append(
            f"| {s.strategy} | {s.intents} | {s.acked} | {s.rejected} | {s.fills} | {s.shares:,.0f} | "
            f"{s.notional:,.0f} | {s.edge_vs_fair:+,.1f} |"
        )
    if not m.strategies:
        lines.append("| (no activity) | | | | | | | |")
    lines += [
        "",
        "## Execution",
        "",
        f"- Fills today: {m.fills}",
        f"- Constraints racer: {m.racer_hits}/{m.racer_attempts} attempts filled "
        f"(hit rate {_f(m.racer_hit_rate, '{:.0%}')}); response latency "
        f"p50 {_f(m.racer_latency_ms_p50, '{:.0f} ms')}, p95 {_f(m.racer_latency_ms_p95, '{:.0f} ms')}",
        f"- Rejects by code: {', '.join(f'{k} × {v}' for k, v in sorted(m.reject_codes.items())) or 'none'}",
        "",
        "## Sponsor spreads",
        "",
        f"- Markets quoted: {m.markets_quoted}; median spread {_f(m.spread_median, '{:.3f}')}, "
        f"p90 {_f(m.spread_p90, '{:.3f}')}; share ≤ 2c wide {_f(m.tight_share, '{:.0%}')}",
        "",
        "## Tournament",
        "",
        f"- Rank {m.my_rank or 'unranked'} of {m.field_size or 'n/a'}; "
        f"top balance {_f(m.top1_multiple, '{:.2f}x')}",
        f"- Fitted winning multiple T: p50 {_f(m.t_p50, '{:.1f}x')}, p90 {_f(m.t_p90, '{:.1f}x')} "
        f"({m.t_method or 'n/a'})",
        f"- Current book: P(1st) {_f(m.p_first, '{:.2%}')}, P(top 3) {_f(m.p_top3, '{:.2%}')}; scenario "
        f"`{m.scenario or 'none'}`; targets {_f(m.targets_age_h, '{:.1f} h')} old",
        f"- Crowd drift (national, D-direction): {_f(m.national_drift_pts, '{:+.1f} pts')}",
        f"- Senate calibration gap: {_f(m.senate_gap_pts, '{:+.1f} pts')}",
        "",
        "## Risk events",
        "",
    ]
    events = (
        m.risk_events
        + m.calibration_warnings
        + ([f"{m.fair_flags} fair-value mapping flags"] if m.fair_flags else [])
    )
    lines += [f"- {e}" for e in events] or ["- none"]
    return "\n".join(lines) + "\n"


def write_daily(reports_dir: Path, m: DailyMetrics) -> Path:
    path = reports_dir / "daily" / f"{m.day.isoformat()}.md"
    atomic_write_text(path, render(m))
    return path
