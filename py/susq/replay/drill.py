"""Election-night drill: replay → needle → simulated executor → `reports/drills/<date>.md`.

What a drill measures (ITERATION_AND_IMPROVEMENTS §2.6):

* **Latency**: wall time from a payload's arrival to its posterior, and to the executor's decision.
* **False triggers**: a posterior crossing the trigger probability for a side that later reverses
  (drops below 0.5) or whose side loses in the replay truth.
* **P&L vs truth** for a rule-based executor trading a simulated Cup book.
* **Chaos behaviour**: rejected malformed payloads, seq resets, and orders suppressed while the
  input was unhealthy (no heartbeat/batch for `stale_after_s`, or the feed flagged down).

Market model: the Cup book is as informed as our posterior but **late**. The YES(D) mid at time t
is our own posterior at `t − market_delay_s` (before that, the pre-election prior), and we pay a
half-spread. This isolates the value of latency; it is not a claim about how the Sponsor quotes.

Executor rule (SPEC §3 Phase 3): buy side s when q_s − ask_s ≥ max(min_edge_pts/100, se_mult × SE),
with at least `min_counties` counties reporting (corroboration). SE is the standard deviation of q
over the last `se_window` posteriors, a direct measure of how settled the estimate is. One
position per race and side, held to resolution.

Count-order gate (EXP-006, optional): with a `CountOrderTable`, a buy against the registered
direction of the count still to come (or in a race with no registered direction) also needs the
band converted to probability through the posterior's margin and sd, as in
`crates/strategies/src/en_executor.rs`. `compare_count_order` runs the same replay with the gate
off and on. The drill's market is our own lagged posterior, not a book that extrapolates the
partial count, so a drill can show what the gate costs; it cannot show the mispricing the gate is
meant to exploit.
"""

from __future__ import annotations

import statistics
import time
from bisect import bisect_right
from collections import deque
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from statistics import NormalDist

from susq.contracts import bus_pb2, needle_pb2
from susq.replay.count_order import CountOrderTable, band_prob
from susq.replay.records import RaceTruth
from susq.replay.runner import ReplayRecord

_PHI = NormalDist()


@dataclass(frozen=True)
class ExecutorConfig:
    min_edge_pts: float = 8.0
    se_mult: float = 3.0
    se_window: int = 5
    min_counties: int = 2
    market_delay_s: float = 600.0
    half_spread: float = 0.01
    qty: int = 100
    trigger_prob: float = 0.85
    stale_after_s: float = 90.0
    resume_after_healthy: int = 3


@dataclass
class Trade:
    t_s: float
    race_id: str
    side: str
    price: float
    q: float
    qty: int
    decision_latency_s: float
    pnl: float = 0.0


@dataclass
class Trigger:
    t_s: float
    race_id: str
    side: str
    false: bool
    reason: str


@dataclass
class DrillResult:
    record: ReplayRecord
    truths: dict[str, RaceTruth]
    trades: list[Trade] = field(default_factory=list)
    triggers: list[Trigger] = field(default_factory=list)
    suppressed_orders: int = 0
    unhealthy_s: float = 0.0
    decision_latencies_s: list[float] = field(default_factory=list)
    gate: str = "off"
    # Race-sides whose first qualifying buy the count-order gate refused (it may buy them later).
    gate_blocked: set[tuple[str, str]] = field(default_factory=set)

    @property
    def pnl(self) -> float:
        return sum(t.pnl for t in self.trades)

    @property
    def false_triggers(self) -> int:
        return sum(t.false for t in self.triggers)


def _price_path(
    record: ReplayRecord, race_id: str, prior: needle_pb2.RacePrior
) -> tuple[list[float], list[float]]:
    times = [s.t_s for s in record.samples if race_id in s.races]
    probs = [s.races[race_id].p_dem for s in record.samples if race_id in s.races]
    p0 = _PHI.cdf(prior.prior_margin_mean / max(prior.prior_margin_sd, 0.1))
    return [float("-inf"), *times], [p0, *probs]


def _market_p_dem(path: tuple[list[float], list[float]], t: float) -> float:
    times, probs = path
    return probs[bisect_right(times, t) - 1]


def _clip(p: float) -> float:
    return min(0.995, max(0.005, p))


def simulate_executor(
    record: ReplayRecord,
    prior: needle_pb2.NeedlePrior,
    truths: dict[str, RaceTruth],
    cfg: ExecutorConfig | None = None,
    count_order: CountOrderTable | None = None,
) -> DrillResult:
    """Run the executor over a replay record (with the count-order gate when a table is given)."""
    cfg = cfg or ExecutorConfig()
    result = DrillResult(record=record, truths=truths, gate="on" if count_order is not None else "off")
    race_priors = {r.race_id: r for r in prior.races}
    paths = {rid: _price_path(record, rid, rp) for rid, rp in race_priors.items()}
    history: dict[tuple[str, str], deque[float]] = {}
    held: set[tuple[str, str]] = set()
    armed: dict[tuple[str, str], Trigger] = {}
    below: dict[tuple[str, str], bool] = {}

    gate = _HealthGate(cfg)
    events = sorted(record.health, key=lambda e: e.t_s)
    ev_i = 0

    for sample in record.samples:
        while ev_i < len(events) and events[ev_i].t_s <= sample.t_s:
            gate.observe(events[ev_i].t_s, events[ev_i].healthy_input)
            ev_i += 1
        healthy = gate.healthy
        for race_id, post in sample.races.items():
            if race_id not in race_priors:
                continue
            for side, q in (("DEM", post.p_dem), ("REP", 1.0 - post.p_dem)):
                key = (race_id, side)
                hist = history.setdefault(key, deque(maxlen=cfg.se_window))
                hist.append(q)
                _track_trigger(result, armed, below, key, q, sample.t_s, cfg)
                if key in held:
                    continue
                t0 = time.perf_counter()
                m = _market_p_dem(paths[race_id], sample.t_s - cfg.market_delay_s)
                ask = _clip((m if side == "DEM" else 1.0 - m) + cfg.half_spread)
                se = statistics.pstdev(hist) if len(hist) >= 2 else 1.0
                edge = q - ask
                wants = (
                    edge >= max(cfg.min_edge_pts / 100.0, cfg.se_mult * se)
                    and post.n_counties_reporting >= cfg.min_counties
                )
                if wants and count_order is not None:
                    extra = _gate_extra(count_order, truths.get(race_id), race_id, side, post, sample.t_s)
                    if edge < max(cfg.min_edge_pts / 100.0, cfg.se_mult * se) + extra:
                        result.gate_blocked.add(key)
                        wants = False
                decision = time.perf_counter() - t0
                if not wants:
                    continue
                if not healthy:
                    result.suppressed_orders += 1
                    continue
                total_latency = sample.latency_s + decision
                result.decision_latencies_s.append(total_latency)
                truth = truths.get(race_id)
                payoff = 1.0 if truth is not None and truth.winner == side else 0.0
                result.trades.append(
                    Trade(sample.t_s, race_id, side, ask, q, cfg.qty, total_latency, cfg.qty * (payoff - ask))
                )
                held.add(key)
    if record.samples:
        gate.finish(record.samples[-1].t_s)
    result.unhealthy_s = gate.unhealthy_s
    # Triggers still armed at the end: false if the side lost.
    for key, trig in armed.items():
        truth = truths.get(key[0])
        if truth is not None and truth.winner != key[1]:
            trig.false = True
            trig.reason = "side lost in truth"
    return result


def _gate_extra(
    table: CountOrderTable,
    truth: RaceTruth | None,
    race_id: str,
    side: str,
    post: bus_pb2.RacePosterior,
    t_s: float,
) -> float:
    """Extra edge the count-order gate asks of a buy of `side` ('DEM'/'REP'): 0 when it agrees with
    the registered direction or the race is unregistered, the band in probability otherwise, and
    infinity when the band cannot be priced (no margin sd), which refuses the buy."""
    if truth is None:
        return 0.0
    profile = table.profile(race_id, truth.state)
    if profile is None:
        return 0.0
    reg = profile.at_hour((t_s - truth.poll_close_offset_s) / 3600.0)
    if reg is None or reg.favours() == side:
        return 0.0
    bp = band_prob(post.margin_mean, post.margin_sd, reg.band_pts, side)
    return float("inf") if bp is None else bp


@dataclass
class Comparison:
    off: DrillResult
    on: DrillResult


def compare_count_order(
    record: ReplayRecord,
    prior: needle_pb2.NeedlePrior,
    truths: dict[str, RaceTruth],
    cfg: ExecutorConfig,
    table: CountOrderTable,
) -> Comparison:
    """The same replay record through the executor with the count-order gate off and on."""
    return Comparison(
        off=simulate_executor(record, prior, truths, cfg),
        on=simulate_executor(record, prior, truths, cfg, count_order=table),
    )


def render_comparison(
    cmp: Comparison,
    *,
    title: str,
    replay: str,
    backend_note: str,
    cfg: ExecutorConfig,
    table: CountOrderTable,
    generated: datetime | None = None,
) -> str:
    """Markdown comparison of gate-off versus gate-on (EXP-006)."""
    when = (generated or datetime.now(UTC)).strftime("%Y-%m-%d %H:%M:%SZ")
    rows = [("Gate off", cmp.off), ("Gate on", cmp.on)]
    covered = sorted({t.state for t in cmp.on.truths.values() if table.profile(t.race_id, t.state)})
    lines = [
        f"# Count-order drill (EXP-006): {title}",
        "",
        f"Generated {when}. Replay `{replay}`, backend **{backend_note}**. Same replay record for both runs; "
        f"market lags our posterior by {cfg.market_delay_s:g} s, half-spread {cfg.half_spread:g}, "
        f"{cfg.qty} contracts per trade.",
        f"Registered states in this replay: {', '.join(covered) or 'none'}.",
        "",
        "| Run | Trades | P&L vs truth | False triggers | Race-sides the gate refused at least once |",
        "|---|---|---|---|---|",
    ]
    for name, r in rows:
        lines.append(
            f"| {name} | {len(r.trades)} | {r.pnl:+.1f} | {r.false_triggers} | {len(r.gate_blocked)} |"
        )
    lines += [
        "",
        "## Trades",
        "",
        "| Run | t (min) | Race | Side | q | Ask | P&L |",
        "|---|---|---|---|---|---|---|",
    ]
    for name, r in rows:
        for t in r.trades:
            lines.append(
                f"| {name} | {t.t_s / 60:.1f} | {t.race_id} | {t.side} | {t.q:.3f} | {t.price:.3f} "
                f"| {t.pnl:+.1f} |"
            )
    if cmp.on.gate_blocked:
        lines += [
            "",
            "Refused by the gate (first qualifying buy): "
            + ", ".join(f"{race} {side}" for race, side in sorted(cmp.on.gate_blocked))
            + ".",
        ]
    lines += [
        "",
        "## What this drill can and cannot show",
        "",
        "The simulated Cup book is our own posterior delayed by `market_delay_s`. It never extrapolates "
        "the partial count, which is the mispricing EXP-006 registers, so this drill measures only what "
        "the gate costs (trades it refuses against the registered direction) and whether those trades "
        "would have won. It is not a test of the hypothesis. The bundled fixture is 2022 only (GA, NH, ME "
        "county totals with synthetic timing from `conventions.py`); there is no 2024 fixture, so the "
        "spec's 2022-and-2024 comparison is not possible with what is in the repo.",
        "",
    ]
    return "\n".join(lines)


class _HealthGate:
    """Input health as the executor sees it: unhealthy after `stale_after_s` without a healthy
    input or on a feed-down signal; healthy again after `resume_after_healthy` consecutive healthy
    inputs (LANGUAGES_AND_BOUNDARIES: never trade on old numbers, resume only after N good ones)."""

    def __init__(self, cfg: ExecutorConfig) -> None:
        self.cfg = cfg
        self.healthy = True
        self.unhealthy_s = 0.0
        self._last_ok: float | None = None
        self._down_since = 0.0
        self._streak = 0

    def _go_down(self, at: float) -> None:
        if self.healthy:
            self.healthy = False
            self._down_since = at
        self._streak = 0

    def observe(self, t: float, ok: bool) -> None:
        if self._last_ok is not None and t - self._last_ok > self.cfg.stale_after_s:
            self._go_down(self._last_ok + self.cfg.stale_after_s)
        if not ok:
            self._go_down(t)
            return
        self._last_ok = t
        self._streak += 1
        if not self.healthy and self._streak >= self.cfg.resume_after_healthy:
            self.healthy = True
            self.unhealthy_s += t - self._down_since

    def finish(self, t: float) -> None:
        if not self.healthy:
            self.unhealthy_s += max(0.0, t - self._down_since)


def _track_trigger(
    result: DrillResult,
    armed: dict[tuple[str, str], Trigger],
    below: dict[tuple[str, str], bool],
    key: tuple[str, str],
    q: float,
    t_s: float,
    cfg: ExecutorConfig,
) -> None:
    if q >= cfg.trigger_prob and below.get(key, True):
        new = Trigger(t_s, key[0], key[1], False, "")
        result.triggers.append(new)
        armed[key] = new
    below[key] = q < cfg.trigger_prob
    live = armed.get(key)
    if live is not None and q < 0.5:
        live.false = True
        live.reason = f"reversed below 0.5 at t={t_s / 60:.0f} min"
        del armed[key]


def _pct(xs: list[float], p: float) -> float:
    if not xs:
        return float("nan")
    s = sorted(xs)
    return s[min(len(s) - 1, int(p * (len(s) - 1) + 0.5))]


def render_report(
    result: DrillResult,
    *,
    title: str,
    replay: str,
    backend_note: str,
    cfg: ExecutorConfig,
    generated: datetime | None = None,
) -> str:
    """Markdown drill report."""
    rec = result.record
    lat_ms = [s.latency_s * 1000 for s in rec.samples]
    dec_ms = [x * 1000 for x in result.decision_latencies_s]
    nan = float("nan")
    when = (generated or datetime.now(UTC)).strftime("%Y-%m-%d %H:%M:%SZ")
    lines = [
        f"# Drill: {title}",
        "",
        f"Generated {when}. Replay `{replay}`, backend **{backend_note}**, "
        f"speed {'max' if rec.speed <= 0 else f'{rec.speed:g}x'}, wall time {rec.wall_s:.2f} s.",
        f"Chaos: {rec.chaos.describe()}.",
        "",
        "## Summary",
        "",
        "| Metric | Value |",
        "|---|---|",
        f"| Batches delivered | {rec.batches_delivered} |",
        f"| Posteriors | {len(rec.samples)} |",
        f"| Latency arrival → posterior p50 / p95 / max | {_pct(lat_ms, 0.5):.2f} / "
        f"{_pct(lat_ms, 0.95):.2f} / {max(lat_ms, default=nan):.2f} ms |",
        f"| Latency arrival → order decision p50 / max | {_pct(dec_ms, 0.5):.2f} / "
        f"{max(dec_ms, default=nan):.2f} ms |",
        f"| Trades | {len(result.trades)} |",
        f"| P&L vs truth | {result.pnl:+.1f} SUSQies |",
        f"| Triggers (q ≥ {cfg.trigger_prob:.2f}) / false | {len(result.triggers)} / "
        f"{result.false_triggers} |",
        f"| Malformed injected / rejected | {rec.malformed_injected} / {rec.malformed_rejected} |",
        f"| Seq resets / gaps | {rec.seq_resets} / {rec.seq_gaps} |",
        f"| Publisher kills / restarts | {rec.kills} / {rec.restarts} |",
        f"| Orders suppressed while input unhealthy | {result.suppressed_orders} |",
        f"| Replay time with input unhealthy | {result.unhealthy_s / 60:.1f} min |",
        "",
        "## Executor",
        "",
        f"Buy when q − ask ≥ max({cfg.min_edge_pts:g} pts, {cfg.se_mult:g} × SE) "
        f"with ≥ {cfg.min_counties} counties; market lags our posterior by {cfg.market_delay_s:g} s; "
        f"half-spread {cfg.half_spread:g}; "
        f"{cfg.qty} contracts per trade; stale after {cfg.stale_after_s:g} s, resume after "
        f"{cfg.resume_after_healthy} healthy inputs.",
        "",
    ]
    if result.trades:
        lines += [
            "| t (min) | Race | Side | q | Ask | P&L | Decision latency (ms) |",
            "|---|---|---|---|---|---|---|",
        ]
        for t in result.trades:
            lines.append(
                f"| {t.t_s / 60:.1f} | {t.race_id} | {t.side} | {t.q:.3f} | {t.price:.3f} | {t.pnl:+.1f} | "
                f"{t.decision_latency_s * 1000:.2f} |"
            )
    else:
        lines.append("No trades.")
    lines += ["", "## Triggers", ""]
    if result.triggers:
        lines += ["| t (min) | Race | Side | False? | Why |", "|---|---|---|---|---|"]
        for tr in result.triggers:
            false = "yes" if tr.false else "no"
            lines.append(f"| {tr.t_s / 60:.1f} | {tr.race_id} | {tr.side} | {false} | {tr.reason} |")
    else:
        lines.append("No triggers.")
    lines += [
        "",
        "## Final posterior vs truth",
        "",
        "| Race | Truth margin | Winner | Final P(D) | Final margin ± sd | Reported |",
    ]
    lines.append("|---|---|---|---|---|---|")
    last = rec.samples[-1].races if rec.samples else {}
    for race_id, truth in sorted(result.truths.items()):
        post = last.get(race_id)
        if post is None:
            lines.append(f"| {race_id} | {truth.margin_pts:+.2f} | {truth.winner} | n/a | n/a | n/a |")
            continue
        lines.append(
            f"| {race_id} | {truth.margin_pts:+.2f} | {truth.winner} | {post.p_dem:.3f} | "
            f"{post.margin_mean:+.2f} ± {post.margin_sd:.2f} | {post.reported_frac:.1%} |"
        )
    lines.append("")
    return "\n".join(lines)


def write_report(text: str, out_dir: Path, date: str | None = None) -> Path:
    """Write `<out_dir>/<date>.md`, suffixing `-2`, `-3`, ... rather than overwriting."""
    out_dir.mkdir(parents=True, exist_ok=True)
    date = date or datetime.now(UTC).strftime("%Y-%m-%d")
    path = out_dir / f"{date}.md"
    n = 2
    while path.exists():
        path = out_dir / f"{date}-{n}.md"
        n += 1
    path.write_text(text)
    return path
