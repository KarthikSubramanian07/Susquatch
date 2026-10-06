"""`python-scheduler`: the long-running research process. It reads, models and writes files; it
never places, cancels or sizes an order (nothing here imports a trading client).

Cadence (docs/RESEARCH.md):

    collect     every 60 s    Kalshi + Polymarket → data/{kalshi,polymarket,reference}
    fair        every 15 s    state/fair_live.json
    telemetry   hourly        state/research/telemetry.json
    targets     daily 06:30 PT, plus hourly checks for a material input change
    report      daily 07:00 PT (the previous complete UTC day)

Every job runs in a worker thread inside its own asyncio task, so a slow or failing job never
blocks another. Exceptions are logged and counted; the job backs off (up to 10 min) and retries.
Job health is written to `state/research/scheduler.json` for the status pusher and reports.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import signal
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from datetime import UTC, date, datetime, timedelta
from datetime import time as dtime
from pathlib import Path
from typing import Annotated, Any
from zoneinfo import ZoneInfo

import typer

from susq.calibrate.consensus import consensus
from susq.calibrate.job import fresh_sim_probs, run_calibration, write_outputs
from susq.calibrate.pipeline import run_targets
from susq.collectors.quotes import RefQuote
from susq.collectors.run import ReferenceCollector, load_latest_quotes
from susq.fair.service import FairService
from susq.mapping.draft import summarize
from susq.mapping.io import load_best_map
from susq.mapping.job import draft_map
from susq.models.facade import get_engine
from susq.reports.job import run_report
from susq.research_env import Paths, ResearchConfig, SeqCounter, atomic_write_text
from susq.telemetry.cupdata import latest_cup_mids
from susq.telemetry.job import run_telemetry

log = logging.getLogger(__name__)

PACIFIC = ZoneInfo("America/Los_Angeles")
MATERIAL_MOVE_PTS = 3.0
MATERIAL_T_CHANGE = 0.25
MAX_BACKOFF_S = 600.0


class ResearchContext:
    """Shared state between jobs: config, paths, venue clients and the latest quotes."""

    def __init__(
        self, paths: Paths, cfg: ResearchConfig, *, clock_ns: Callable[[], int] = time.time_ns
    ) -> None:
        self.paths, self.cfg, self.clock_ns = paths, cfg, clock_ns
        self._collector: ReferenceCollector | None = None
        self._quotes: list[RefQuote] = []
        self._targets_basis: tuple[dict[tuple[str, str], float], float | None] | None = None

    # ── inputs ──────────────────────────────────────────────────────────
    def quotes(self) -> list[RefQuote]:
        return self._quotes or load_latest_quotes(self.paths.data)

    def collector(self) -> ReferenceCollector:
        if self._collector is None:
            self._collector = ReferenceCollector(self.paths.data)
        return self._collector

    # ── jobs ────────────────────────────────────────────────────────────
    def collect(self) -> str:
        res = self.collector().collect_once()
        if res.quotes:
            self._quotes = res.quotes
        return f"{len(res.quotes)} quotes; errors={res.errors or 'none'}"

    def fair(self) -> str:
        svc = FairService(
            out_path=self.paths.fair_live,
            flags_path=self.paths.research_state / "fair_flags.json",
            seq=SeqCounter(self.paths.research_state / "seq-fair.txt"),
            cfg=self.cfg.fair,
            load_rules=lambda: list(mm.markets) if (mm := load_best_map(self.paths)) else [],
            load_quotes=self.quotes,
            load_sim=lambda: fresh_sim_probs(self.paths.research_state, now_ns=self.clock_ns()),
            load_cup_mids=lambda: latest_cup_mids(self.paths.data / "cup"),
            clock_ns=self.clock_ns,
        )
        n_fair, n_flags = svc.run_once()
        return f"{n_fair} fair values, {n_flags} flags"

    def telemetry(self) -> str:
        doc = run_telemetry(self.paths, self.cfg, now_ns=self.clock_ns())
        lb = doc.get("leaderboard") or {}
        return f"field={lb.get('field_size')} top1={lb.get('top1_multiple')}"

    def calibrate(self) -> str:
        now = self.clock_ns()
        res = run_calibration(self.quotes(), get_engine(), market_map=load_best_map(self.paths), now_ns=now)
        write_outputs(res, self.paths.research_state, produced_at_ns=now)
        return json.dumps(res.summary()["chambers"])

    def targets(self, *, require_verified: bool = True) -> str:
        quotes = self.quotes()
        run = run_targets(
            self.paths, self.cfg, quotes, now_ns=self.clock_ns(), require_verified=require_verified
        )
        self._targets_basis = (self._basis(quotes), self._t_p50())
        return f"{run.path.name if run.path else 'not written'}: {run.optimizer.explanation}"

    def targets_if_changed(self) -> str:
        if self._targets_basis is None:
            return "no baseline yet (waiting for the daily run)"
        cons, t50 = self._targets_basis
        now_cons = self._basis(self.quotes())
        moves = [abs(now_cons[k] - v) * 100 for k, v in cons.items() if k in now_cons]
        max_move = max(moves, default=0.0)
        new_t = self._t_p50()
        t_moved = t50 is not None and new_t is not None and abs(new_t / t50 - 1) > MATERIAL_T_CHANGE
        if max_move > MATERIAL_MOVE_PTS or t_moved:
            return f"material change (max move {max_move:.1f} pts, T moved={t_moved}): " + self.targets()
        return f"no material change (max move {max_move:.1f} pts)"

    def report(self, day: date | None = None) -> str:
        now = self.clock_ns()
        day = day or (datetime.fromtimestamp(now / 1e9, tz=UTC).date() - timedelta(days=1))
        daily, packet = run_report(self.paths, self.cfg, day, now_ns=now)
        return f"{daily} + {packet}"

    def map_draft(self, *, promote: bool = False) -> str:
        mm, live = draft_map(self.paths, self.cfg, self.quotes(), now_ns=self.clock_ns(), promote=promote)
        return f"{summarize(mm)}; live map {'written' if live else 'untouched (draft only)'}"

    # ── helpers ─────────────────────────────────────────────────────────
    def _basis(self, quotes: list[RefQuote]) -> dict[tuple[str, str], float]:
        return {k: c.p for k, c in consensus(quotes).items()}

    def _t_p50(self) -> float | None:
        path = self.paths.research_state / "telemetry.json"
        if not path.exists():
            return None
        th = ((json.loads(path.read_text()).get("leaderboard") or {}).get("threshold")) or {}
        p50 = th.get("p50")
        return float(p50) if p50 is not None else None

    def close(self) -> None:
        if self._collector is not None:
            self._collector.close()


@dataclass
class Job:
    """A named job on an interval or at a local wall-clock time each day."""

    name: str
    fn: Callable[[], str]
    interval_s: float | None = None
    daily_at: dtime | None = None
    tz: ZoneInfo = PACIFIC
    run_at_start: bool = True

    def next_run(self, now: datetime) -> datetime:
        if self.interval_s is not None:
            return now + timedelta(seconds=self.interval_s)
        assert self.daily_at is not None
        local = now.astimezone(self.tz)
        candidate = datetime.combine(local.date(), self.daily_at, tzinfo=self.tz)
        if candidate <= local:
            candidate += timedelta(days=1)
        return candidate.astimezone(UTC)


@dataclass
class JobHealth:
    runs: int = 0
    failures: int = 0
    consecutive_failures: int = 0
    last_ok_utc: str = ""
    last_error: str = ""
    last_result: str = ""
    next_run_utc: str = ""


@dataclass
class Scheduler:
    jobs: list[Job]
    health_path: Path
    now: Callable[[], datetime] = field(default=lambda: datetime.now(UTC))
    health: dict[str, JobHealth] = field(default_factory=dict)

    async def run(self, stop: asyncio.Event) -> None:
        tasks = [asyncio.create_task(self._loop(job, stop), name=job.name) for job in self.jobs]
        await stop.wait()
        for t in tasks:
            t.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)

    async def run_once(self, job: Job) -> bool:
        """Run `job` in a worker thread; never raises. Returns True on success."""
        h = self.health.setdefault(job.name, JobHealth())
        h.runs += 1
        try:
            h.last_result = await asyncio.to_thread(job.fn)
        except Exception as exc:
            h.failures += 1
            h.consecutive_failures += 1
            h.last_error = f"{self.now().isoformat()} {type(exc).__name__}: {exc}"
            log.exception("job %s failed", job.name)
            ok = False
        else:
            h.consecutive_failures = 0
            h.last_ok_utc = self.now().isoformat()
            log.info("job %s: %s", job.name, h.last_result)
            ok = True
        self._write_health()
        return ok

    async def _loop(self, job: Job, stop: asyncio.Event) -> None:
        due = self.now() if job.run_at_start else job.next_run(self.now())
        while not stop.is_set():
            self.health.setdefault(job.name, JobHealth()).next_run_utc = due.isoformat()
            delay = (due - self.now()).total_seconds()
            if delay > 0:
                with contextlib.suppress(TimeoutError):
                    await asyncio.wait_for(stop.wait(), timeout=delay)
                continue
            if await self.run_once(job):
                due = job.next_run(self.now())
            else:  # retry sooner than tomorrow for daily jobs, later than usual for interval jobs
                due = self.now() + timedelta(seconds=self.retry_delay_s(job))

    def retry_delay_s(self, job: Job) -> float:
        failures = self.health[job.name].consecutive_failures
        backoff = min(MAX_BACKOFF_S, 5.0 * 2.0 ** min(failures, 7))
        return max(backoff, job.interval_s or 0.0)

    def _write_health(self) -> None:
        doc: dict[str, Any] = {
            "written_utc": self.now().isoformat(),
            "jobs": {k: asdict(v) for k, v in self.health.items()},
        }
        try:
            atomic_write_text(self.health_path, json.dumps(doc, indent=1))
        except OSError:
            log.exception("could not write scheduler health")


def default_jobs(ctx: ResearchContext) -> list[Job]:
    cfg = ctx.cfg
    return [
        Job("collect", ctx.collect, interval_s=cfg.price_snapshot_interval_s),
        Job("fair", ctx.fair, interval_s=15.0),
        # Not at start: a telemetry run that dies (it was OOM-killed at boot for 35 h on Oct 5-6) must not
        # take the collect and fair jobs down with it every restart.
        Job("telemetry", ctx.telemetry, interval_s=cfg.leaderboard_interval_s, run_at_start=False),
        Job("targets", ctx.targets, daily_at=dtime(6, 30), run_at_start=False),
        Job("targets-if-changed", ctx.targets_if_changed, interval_s=3600.0, run_at_start=False),
        Job("report", ctx.report, daily_at=dtime(7, 0), run_at_start=False),
    ]


# ── CLI ─────────────────────────────────────────────────────────────────────
cli_app = typer.Typer(no_args_is_help=True, help="Research jobs (read-only; never trades).")


def _ctx() -> ResearchContext:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
    logging.getLogger("httpx").setLevel(logging.WARNING)
    paths = Paths.from_env()
    return ResearchContext(paths, paths.load_config())


@cli_app.command()
def collect() -> None:
    """Snapshot Kalshi + Polymarket once (live public endpoints)."""
    ctx = _ctx()
    try:
        typer.echo(ctx.collect())
    finally:
        ctx.close()


@cli_app.command("map-draft")
def map_draft_cmd(
    promote: bool = typer.Option(False, help="Also overwrite the live map (verified rows kept)."),
) -> None:
    """Draft state/market_map.json from the newest recorded Cup listing (never verifies rows)."""
    typer.echo(_ctx().map_draft(promote=promote))


@cli_app.command("rules-review")
def rules_review_cmd() -> None:
    """Write reports/rules_review.md: every market, risky races first, with what needs a look."""
    from susq.mapping.io import read_market_map
    from susq.mapping.review import render

    paths = _ctx().paths
    mm = read_market_map(paths.market_map)
    if mm is None:
        raise typer.BadParameter("no state/market_map.json yet: run `susq research map-draft` first")
    out = paths.home / "reports" / "rules_review.md"
    atomic_write_text(out, render(mm))
    typer.echo(f"wrote {out}")


@cli_app.command("verify")
def verify_cmd(
    ids: Annotated[
        list[str] | None, typer.Argument(help="Race ids (2026-SEN-NE) or market ids (191) to approve.")
    ] = None,
    clean: bool = typer.Option(False, "--clean", help="Approve every clean two-party race."),
) -> None:
    """HUMAN ONLY: mark market-map rows verified so strategies may trade them."""
    from susq.mapping.io import read_market_map, write_market_map
    from susq.mapping.review import clean_races, render, verify

    ctx = _ctx()
    paths = ctx.paths
    mm = read_market_map(paths.market_map)
    if mm is None:
        raise typer.BadParameter("no state/market_map.json yet: run `susq research map-draft` first")
    ids = ids or []
    race_ids = [i for i in ids if i.startswith("20")] + (clean_races(mm) if clean else [])
    market_ids = [i for i in ids if not i.startswith("20")]
    if not race_ids and not market_ids:
        raise typer.BadParameter("name races or market ids, or pass --clean")
    try:
        changed = verify(mm, market_ids=market_ids, race_ids=race_ids)
    except KeyError as exc:
        raise typer.BadParameter(str(exc.args[0])) from exc
    mm.header.seq = SeqCounter(paths.research_state / "seq-market-map.txt").next()
    mm.header.produced_at_ns = ctx.clock_ns()
    write_market_map(paths.market_map, mm)
    atomic_write_text(paths.home / "reports" / "rules_review.md", render(mm))
    total = sum(r.verified for r in mm.markets)
    typer.echo(f"verified {len(changed)} more rows; {total} of {len(mm.markets)} rows now verified")


@cli_app.command()
def calibrate() -> None:
    """Fit the simulator to the latest reference mids and audit the chambers."""
    typer.echo(_ctx().calibrate())


@cli_app.command()
def fair() -> None:
    """Write state/fair_live.json once."""
    typer.echo(_ctx().fair())


@cli_app.command()
def telemetry() -> None:
    """Summarize leaderboard + crowd drift into state/research/telemetry.json."""
    typer.echo(_ctx().telemetry())


@cli_app.command()
def targets(
    allow_unverified: bool = typer.Option(False, help="Research only: let unverified rows into the book."),
) -> None:
    """Calibrate, optimize and write a versioned target file + current.json."""
    typer.echo(_ctx().targets(require_verified=not allow_unverified))


@cli_app.command()
def report(day: str = typer.Option("", help="UTC date YYYY-MM-DD (default: yesterday).")) -> None:
    """Write reports/daily/<date>.md and the scrubbed review packet."""
    typer.echo(_ctx().report(date.fromisoformat(day) if day else None))


@cli_app.command()
def run() -> None:
    """Run the long-lived scheduler until SIGINT/SIGTERM."""
    ctx = _ctx()
    sched = Scheduler(default_jobs(ctx), ctx.paths.research_state / "scheduler.json")

    async def main() -> None:
        stop = asyncio.Event()
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.add_signal_handler(sig, stop.set)
        await sched.run(stop)

    try:
        asyncio.run(main())
    finally:
        ctx.close()


if __name__ == "__main__":  # `python -m susq.scheduler targets` inside the container
    cli_app()
