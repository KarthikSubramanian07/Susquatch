"""Deterministic "Top 3 proposed changes" for the daily report (ITERATION §3).

Each rule inspects the day's metrics and either proposes one change or stays silent. Rules carry
a fixed priority, so the same metrics always yield the same three proposals in the same order.
The thresholds are the SPEC §4 decision table and the calibration contract, not tuned numbers.
Proposals are advice for a human; nothing here changes config or trades.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass

from susq.reports.metrics import DailyMetrics


@dataclass(frozen=True)
class Proposal:
    rule_id: str
    priority: int
    text: str


Rule = Callable[[DailyMetrics], Proposal | None]


def _calibration_gap(m: DailyMetrics) -> Proposal | None:
    gap_bad = m.senate_gap_pts is not None and abs(m.senate_gap_pts) > 4
    if not (m.calibration_warnings or gap_bad):
        return None
    gap = f"{m.senate_gap_pts:+.1f} pts" if m.senate_gap_pts is not None else "(see calibration.json)"
    return Proposal(
        rule_id="calibration-gap",
        priority=100,
        text=(
            f"Senate control implied by the race sim is off the market by {gap}. Assume a bug before "
            "assuming edge: re-check race targets, the seat table and the NE caucus assumption "
            "before trading chamber markets."
        ),
    )


def _mapping_flags(m: DailyMetrics) -> Proposal | None:
    if not m.fair_flags:
        return None
    return Proposal(
        rule_id="mapping-flags",
        priority=95,
        text=(
            f"{m.fair_flags} market(s) sit more than mapping_error_pts from fair: run /parse-rules "
            "on them; they stay blocked until verified."
        ),
    )


def _stale_targets(m: DailyMetrics) -> Proposal | None:
    if not any("targets are" in e or "targets unreadable" in e for e in m.risk_events):
        return None
    return Proposal(
        rule_id="stale-targets",
        priority=90,
        text="Target file is stale or unreadable: re-run `susq research targets`.",
    )


def _rejects(m: DailyMetrics) -> Proposal | None:
    total = sum(m.reject_codes.values())
    if total < 10:
        return None
    code, count = max(m.reject_codes.items(), key=lambda kv: (kv[1], kv[0]))
    return Proposal(
        rule_id="rejects",
        priority=85,
        text=f"{total} rejected orders today (most common: {code} × {count}): fix before scaling.",
    )


def _scenario_caps(m: DailyMetrics) -> Proposal | None:
    share = m.scenario_refused_share
    if share is None or share <= 0.5:
        return None
    top = ", ".join(f"{c} {u:.0%}" for c, u in m.cluster_use) or "not in packet"
    gross = "not in packet" if m.total_gross_frac is None else f"{m.total_gross_frac:.3f} of equity"
    return Proposal(
        rule_id="scenario-caps",
        priority=88,
        text=(
            f"{m.scenario_refused_caps} of {m.scenario_orders} scenario orders ({share:.0%}) were refused "
            f"on risk caps (total gross {gross}; closest clusters to their caps: {top}). The book is "
            "cap-limited, not cash-limited: free room under the binding caps or resize the legs."
        ),
    )


def _verify_rows(m: DailyMetrics) -> Proposal | None:
    if not m.unverified_high_confidence:
        return None
    return Proposal(
        rule_id="verify-map",
        priority=80,
        text=(
            f"{m.unverified_high_confidence} high-confidence map rows are unverified; spot-check and "
            "verify them so the sniper and scenario executor may trade them."
        ),
    )


def _racer(m: DailyMetrics) -> Proposal | None:
    if m.racer_attempts < 10 or (m.racer_hit_rate or 0.0) >= 0.2:
        return None
    lat = "" if m.racer_latency_ms_p95 is None else f", p95 latency {m.racer_latency_ms_p95:.0f} ms"
    return Proposal(
        rule_id="racer-slow",
        priority=70,
        text=(
            f"Constraints racer filled {m.racer_hits}/{m.racer_attempts} attempts{lat}: we are losing "
            "the race; prefer realtime triggers and trim request work before the order."
        ),
    )


def _crowd(m: DailyMetrics) -> Proposal | None:
    d = m.national_drift_pts
    if d is None or abs(d) < 2.0:
        return None
    lean, favor = ("D", "R beats polls") if d > 0 else ("R", "D beats polls")
    return Proposal(
        rule_id="crowd-drift",
        priority=65,
        text=(
            f"Cup prices drift {abs(d):.1f} pts more {lean} than real money: the field is long {lean}; "
            f"tilt scenario selection toward '{favor}' (decision table row 5)."
        ),
    )


def _sponsor(m: DailyMetrics) -> Proposal | None:
    if m.spread_median is None or m.markets_quoted < 5:
        return None
    spread_c = m.spread_median * 100
    if m.spread_median <= 0.02 + 1e-9:
        return Proposal(
            rule_id="sponsor-tight",
            priority=60,
            text=(
                f"Median Cup spread {spread_c:.1f}c across {m.markets_quoted} markets: the Sponsor quotes "
                "tight; keep market making off and run the sniper only (decision table row 1)."
            ),
        )
    if m.spread_median >= 0.05:
        return Proposal(
            rule_id="sponsor-wide",
            priority=55,
            text=(
                f"Median Cup spread {spread_c:.1f}c: books are wide; enable market making around fair "
                "(decision table row 2)."
            ),
        )
    return None


def _mark_inflation(m: DailyMetrics) -> Proposal | None:
    early_october = m.day.month == 10 and m.day.day <= 15
    if m.top1_multiple is None or m.top1_multiple < 3.0 or not early_october:
        return None
    return Proposal(
        rule_id="mark-inflation",
        priority=50,
        text=(
            f"Top balance is already {m.top1_multiple:.1f}x in mid-October: investigate illiquid marks "
            "before reading anything into it; don't chase mark-to-market rank (decision table row 6)."
        ),
    )


RULES: tuple[Rule, ...] = (
    _calibration_gap,
    _mapping_flags,
    _stale_targets,
    _rejects,
    _scenario_caps,
    _verify_rows,
    _racer,
    _crowd,
    _sponsor,
    _mark_inflation,
)


def propose(m: DailyMetrics, *, top: int = 3) -> list[Proposal]:
    """The `top` highest-priority proposals (ties broken by rule id)."""
    found = [p for rule in RULES if (p := rule(m)) is not None]
    found.sort(key=lambda p: (-p.priority, p.rule_id))
    return found[:top]
