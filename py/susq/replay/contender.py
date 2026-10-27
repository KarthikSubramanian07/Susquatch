"""EXP-014 contender drill: the equity-multiple path of the concentrated night profile on a replay.

`python -m susq.replay drill-contender <spec> --seeds N` replays a spec once through the needle, then
runs N seeded simulations of the election-night executor over the same posterior record, each with
its own market model, and writes `reports/drills/<date>-contender.md`: the multiple distribution,
how many runs reached the target multiple, the path of the best run, every refusal while the input
was unhealthy, and the depth each roll consumed against what was there.

It mirrors the Rust executor (`crates/strategies/src/en_executor.rs`, `[strategies.en]`) in both
profiles, so one report shows the default (25% budget, 5% per market, 2-point roll) beside contender
mode (budget, deep-book market cap, hold to the call, chain ranked by edge × depth):

* buy a side when `q − ask ≥ max(min_edge_pts, se_mult × SE)` with at least `min_counties`
  counties reporting; SE is the delta-method margin SE the Rust executor uses
  (`phi(m / sd) × se_margin_err_pts / sd`), falling back to the spread of recent posteriors;
* default exit: bid within `roll_within_pts` of q. Contender exit: bid ≥ `exit_bid`, or SE ≤
  `call_se` and bid ≥ q − `min_edge_pts`;
* every order is at most `max_order_frac` of equity and at most the depth left on that side of the
  book; at most `orders_per_step` orders per race and posterior (rust-live re-decides every tick,
  a posterior lasts many ticks);
* a market may hold `contender_market_frac` of equity only while its book holds `min_depth` near
  the touch, else `per_market_frac`; deployment at cost is capped at the night budget;
* nothing is sent while the input is unhealthy (the needle watchdog), and each would-be order then
  is counted as a refusal.

Market model (per seed): each race's YES(D) mid follows our own posterior delayed by a random
`market_delay_s` in `[delay_lo_s, delay_hi_s]`, plus Gaussian noise of `noise_pts`, with
`half_spread` either side. Depth near the touch is lognormal around `median_depth` per race and
side, and refills after each sample (the field keeps quoting). The model has the same limit as the
drill in `drill.py`: the Cup book is as informed as our posterior, only late, so this measures what
latency and the profile's rules can compound to, not the needle's real edge over the field.
"""

from __future__ import annotations

import math
import random
from dataclasses import dataclass, field
from datetime import UTC, datetime
from statistics import NormalDist

from susq.contracts import needle_pb2
from susq.replay.drill import _HealthGate, _pct, _price_path
from susq.replay.records import RaceTruth
from susq.replay.runner import ReplayRecord

_PHI = NormalDist()
STARTING_BALANCE = 100_000.0


@dataclass(frozen=True)
class ContenderConfig:
    """The executor's knobs (`[strategies.en]` / `[risk]` names) and the market model's."""

    contender: bool = True
    min_edge_pts: float = 8.0
    se_mult: float = 3.0
    se_margin_err_pts: float = 1.0
    min_counties: int = 2
    roll_within_pts: float = 2.0
    max_order_frac: float = 0.02
    per_market_frac: float = 0.05
    night_budget_frac: float = 0.25
    contender_budget_frac: float = 0.80
    exit_bid: float = 0.93
    call_se: float = 0.005
    contender_market_frac: float = 0.30
    min_depth: float = 100_000.0
    orders_per_step: int = 10
    start_multiple: float = 0.98
    target_multiple: float = 10.0
    # Market model.
    delay_lo_s: float = 300.0
    delay_hi_s: float = 1800.0
    noise_pts: float = 1.0
    half_spread: float = 0.01
    median_depth: float = 134_000.0
    depth_sigma: float = 1.0
    stale_after_s: float = 90.0
    resume_after_healthy: int = 3

    @property
    def budget_frac(self) -> float:
        return self.contender_budget_frac if self.contender else self.night_budget_frac


@dataclass
class Roll:
    """One completed exit: what was sold, at what bid, against the depth that was there."""

    t_s: float
    race_id: str
    side: str
    why: str
    shares: float
    bid: float
    notional: float
    depth: float


@dataclass
class RunResult:
    seed: int
    contender: bool
    path: list[tuple[float, float]] = field(default_factory=list)  # (t_s, equity multiple)
    rolls: list[Roll] = field(default_factory=list)
    buys: int = 0
    refused_unhealthy: int = 0
    max_order_over_depth: float = 0.0  # largest order / depth on its side (must stay ≤ 1)
    final_multiple: float = 0.0

    @property
    def max_multiple(self) -> float:
        return max((m for _, m in self.path), default=self.final_multiple)


@dataclass
class _Pos:
    shares: float = 0.0
    cost: float = 0.0


def _se(post: object, q: float, hist: list[float], err_pts: float) -> float:
    m = float(getattr(post, "margin_mean", 0.0))
    sd = float(getattr(post, "margin_sd", 0.0))
    if sd > 0.0 and math.isfinite(m) and err_pts > 0.0:
        return math.exp(-0.5 * (m / sd) ** 2) / math.sqrt(2 * math.pi) * err_pts / sd
    if len(hist) >= 2:
        mean = sum(hist) / len(hist)
        return math.sqrt(sum((x - mean) ** 2 for x in hist) / len(hist))
    return math.sqrt(max(q * (1 - q), 0.0))


def simulate_contender(
    record: ReplayRecord,
    prior: needle_pb2.NeedlePrior,
    truths: dict[str, RaceTruth],
    cfg: ContenderConfig,
    seed: int,
) -> RunResult:
    """One seeded run of the executor over a replay record."""
    rng = random.Random(seed)  # noqa: S311 - a simulation seed, not a secret
    race_priors = {r.race_id: r for r in prior.races}
    paths = {rid: _price_path(record, rid, rp) for rid, rp in race_priors.items()}
    delays = {rid: rng.uniform(cfg.delay_lo_s, cfg.delay_hi_s) for rid in sorted(race_priors)}
    depth = {
        (rid, side): cfg.median_depth * math.exp(rng.gauss(0.0, cfg.depth_sigma))
        for rid in sorted(race_priors)
        for side in ("DEM", "REP")
    }
    result = RunResult(seed=seed, contender=cfg.contender)
    cash = cfg.start_multiple * STARTING_BALANCE
    held: dict[tuple[str, str], _Pos] = {}
    hist: dict[tuple[str, str], list[float]] = {}
    gate = _HealthGate(cfg)  # type: ignore[arg-type]  # reads stale_after_s / resume_after_healthy
    events = sorted(record.health, key=lambda e: e.t_s)
    ev_i = 0
    noise: dict[tuple[str, float], float] = {}

    def mid(rid: str, t: float) -> float:
        times, probs = paths[rid]
        i = max(0, _bisect(times, t - delays[rid]))
        key = (rid, times[i])
        if key not in noise:
            noise[key] = rng.gauss(0.0, cfg.noise_pts / 100.0)
        return min(0.99, max(0.01, probs[i] + noise[key]))

    def side_mid(rid: str, side: str, t: float) -> float:
        m = mid(rid, t)
        return m if side == "DEM" else 1.0 - m

    def equity(t: float) -> float:
        return cash + sum(p.shares * side_mid(rid, side, t) for (rid, side), p in held.items())

    for sample in record.samples:
        t = sample.t_s
        while ev_i < len(events) and events[ev_i].t_s <= t:
            gate.observe(events[ev_i].t_s, events[ev_i].healthy_input)
            ev_i += 1
        eq = equity(t)
        budget = cfg.budget_frac * eq
        deployed = sum(p.cost for p in held.values())
        candidates: list[tuple[float, str, str, float, float, float]] = []
        for rid, post in sorted(sample.races.items()):
            if rid not in race_priors:
                continue
            for side, q in (("DEM", post.p_dem), ("REP", 1.0 - post.p_dem)):
                key = (rid, side)
                h = hist.setdefault(key, [])
                h.append(q)
                del h[:-5]
                se = _se(post, q, h, cfg.se_margin_err_pts)
                m = side_mid(rid, side, t)
                bid, ask = max(0.005, m - cfg.half_spread), min(0.995, m + cfg.half_spread)
                pos = held.get(key)
                if pos is not None and pos.shares > 0:
                    if cfg.contender:
                        why = (
                            "exit_bid"
                            if bid >= cfg.exit_bid
                            else "called"
                            if se <= cfg.call_se and bid >= max(q - cfg.min_edge_pts / 100.0, 0.005)
                            else ""
                        )
                    else:
                        why = "roll" if bid >= q - cfg.roll_within_pts / 100.0 else ""
                    if why:
                        if not gate.healthy:
                            result.refused_unhealthy += 1
                            continue
                        room = depth[key]  # the side's book near the touch
                        shares = min(pos.shares, room / bid)
                        result.max_order_over_depth = max(result.max_order_over_depth, shares * bid / room)
                        cash += shares * bid
                        frac = shares / pos.shares
                        deployed -= pos.cost * frac
                        pos.cost *= 1 - frac
                        pos.shares -= shares
                        result.rolls.append(Roll(t, rid, side, why, shares, bid, shares * bid, room))
                        if pos.shares <= 1e-9:
                            del held[key]
                        continue
                edge = q - ask
                threshold = max(cfg.min_edge_pts / 100.0, cfg.se_mult * se)
                if edge + 1e-9 >= threshold and post.n_counties_reporting >= cfg.min_counties:
                    candidates.append((edge * depth[key], rid, side, ask, q, threshold))
        if cfg.contender:
            candidates.sort(key=lambda c: -c[0])
        for _, rid, side, ask, _q, _thr in candidates:
            if not gate.healthy:
                result.refused_unhealthy += 1
                continue
            key = (rid, side)
            avail = depth[key]
            deep = avail >= cfg.min_depth
            cap_frac = (
                max(cfg.contender_market_frac, cfg.per_market_frac)
                if cfg.contender and deep
                else cfg.per_market_frac
            )
            in_market = sum(p.shares * side_mid(r, s, t) for (r, s), p in held.items() if r == rid)
            spent = 0.0
            for _ in range(cfg.orders_per_step):
                notional = min(
                    cfg.max_order_frac * eq,
                    cap_frac * eq - in_market - spent,
                    budget - deployed - spent,
                    cash - spent,
                    avail - spent,
                )
                if notional <= 1.0:
                    break
                result.max_order_over_depth = max(result.max_order_over_depth, notional / avail)
                spent += notional
                result.buys += 1
            if spent > 0:
                pos = held.setdefault(key, _Pos())
                pos.shares += spent / ask
                pos.cost += spent
                cash -= spent
                deployed += spent
        result.path.append((t, equity(t) / STARTING_BALANCE))
    if record.samples:
        gate.finish(record.samples[-1].t_s)
    # Resolution: each held side pays 1 if it won in the replay truth.
    for (rid, side), pos in held.items():
        truth = truths.get(rid)
        if truth is not None and truth.winner == side:
            cash += pos.shares
    result.final_multiple = cash / STARTING_BALANCE
    t_end = record.samples[-1].t_s if record.samples else 0.0
    result.path.append((t_end, result.final_multiple))
    return result


def _bisect(times: list[float], t: float) -> int:
    lo, hi = 0, len(times)
    while lo < hi:
        mid = (lo + hi) // 2
        if times[mid] <= t:
            lo = mid + 1
        else:
            hi = mid
    return lo - 1


@dataclass
class ContenderDrill:
    contender: list[RunResult]
    default: list[RunResult]


def run_seeds(
    record: ReplayRecord,
    prior: needle_pb2.NeedlePrior,
    truths: dict[str, RaceTruth],
    cfg: ContenderConfig,
    seeds: int,
) -> ContenderDrill:
    """Both profiles on the same seeds (same market model per seed)."""
    off = ContenderConfig(**{**cfg.__dict__, "contender": False})
    on = ContenderConfig(**{**cfg.__dict__, "contender": True})
    return ContenderDrill(
        contender=[simulate_contender(record, prior, truths, on, s) for s in range(seeds)],
        default=[simulate_contender(record, prior, truths, off, s) for s in range(seeds)],
    )


def render_contender(
    drill: ContenderDrill,
    *,
    title: str,
    replay: str,
    backend_note: str,
    cfg: ContenderConfig,
    generated: datetime | None = None,
) -> str:
    """Markdown report: multiple distribution per profile, the best contender path, refusals."""
    when = (generated or datetime.now(UTC)).strftime("%Y-%m-%d %H:%M:%SZ")
    lines = [
        f"# Contender drill (EXP-014): {title}",
        "",
        f"Generated {when}. Replay `{replay}`, backend **{backend_note}**, {len(drill.contender)} seeds. "
        f"Start {cfg.start_multiple:g}x; market lags our posterior by "
        f"{cfg.delay_lo_s:g}-{cfg.delay_hi_s:g} s (per race and seed), noise {cfg.noise_pts:g} pts, "
        f"half-spread {cfg.half_spread:g}, median depth "
        f"{cfg.median_depth:,.0f} near the touch. A model of the shape, not a forecast (see module docs).",
        "",
        "| Profile | p10 | p50 | p90 | max | runs ≥ "
        f"{cfg.target_multiple:g}x | rolls / run | refusals while unhealthy | max order / depth |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    for name, runs in (("Contender", drill.contender), ("Default", drill.default)):
        finals = [r.final_multiple for r in runs]
        hits = sum(r.final_multiple >= cfg.target_multiple for r in runs)
        rolls = sum(len(r.rolls) for r in runs) / max(len(runs), 1)
        refused = sum(r.refused_unhealthy for r in runs)
        over = max((r.max_order_over_depth for r in runs), default=0.0)
        lines.append(
            f"| {name} | {_pct(finals, 0.1):.2f}x | {_pct(finals, 0.5):.2f}x | {_pct(finals, 0.9):.2f}x | "
            f"{max(finals, default=0.0):.2f}x | {hits} / {len(runs)} | {rolls:.1f} | {refused} | {over:.2f} |"
        )
    best = max(drill.contender, key=lambda r: r.final_multiple, default=None)
    if best is not None:
        lines += [
            "",
            f"## Best contender run (seed {best.seed}): {best.final_multiple:.2f}x final, "
            f"{best.max_multiple:.2f}x peak",
            "",
            "| t (min) | Race | Side | Exit | Shares | Bid | Notional | Depth there |",
            "|---|---|---|---|---|---|---|---|",
        ]
        for r in best.rolls:
            lines.append(
                f"| {r.t_s / 60:.1f} | {r.race_id} | {r.side} | {r.why} | {r.shares:,.0f} | {r.bid:.3f} | "
                f"{r.notional:,.0f} | {r.depth:,.0f} |"
            )
        step = max(1, len(best.path) // 20)
        lines += ["", "Multiple path: " + ", ".join(f"{m:.2f}" for _, m in best.path[::step]) + "."]
    lines += [
        "",
        "Pass bars for rehearsal #2 (EXP-014): zero refusals turned into orders while the needle was down "
        "(the column counts orders the watchdog stopped), max order / depth ≤ 1.00, and at least one run "
        f"≥ {cfg.target_multiple:g}x before arming is considered.",
        "",
    ]
    return "\n".join(lines)
