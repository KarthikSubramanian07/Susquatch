"""End-to-end research run (offline) and the scheduler's robustness."""

from __future__ import annotations

import asyncio
import json
from datetime import UTC, datetime, time, timedelta

import polars as pl
import pytest
from susq.calibrate.pipeline import run_targets
from susq.calibrate.targets import read_current
from susq.collectors.store import write_snapshot
from susq.contracts import to_json
from susq.mapping.io import read_market_map
from susq.mapping.job import draft_map
from susq.research_env import ResearchConfig
from susq.scheduler import Job, ResearchContext, Scheduler, default_jobs

from .conftest import FIXTURES, NOW_NS, load


def _prepare_home(home, quotes, *, verify: bool) -> None:
    write_snapshot(home.data / "reference", [q.as_row() for q in quotes], NOW_NS)
    cup = home.data / "cup"
    (cup / "markets").mkdir(parents=True)
    (cup / "markets" / "markets-20260929T000000Z.json").write_text(
        (FIXTURES / "cup_markets.json").read_text()
    )
    ts = datetime.fromtimestamp(NOW_NS / 1e9, tz=UTC)
    rows = [
        {"ts": ts, "exchange_id": x["id"], "market_id": m["id"], "latest_price": x["latestPrice"],
         "best_bid": max(0.005, x["latestPrice"] - 0.01), "best_ask": min(0.995, x["latestPrice"] + 0.01),
         "spread": 0.02}
        for page in load("cup_markets.json") for m in page["data"] for x in m["exchanges"]
    ]  # fmt: skip
    (cup / "prices").mkdir()
    pl.DataFrame(rows).with_columns(pl.col("ts").cast(pl.Datetime("us", "UTC"))).write_parquet(
        cup / "prices" / "p.parquet"
    )
    draft_map(home, ResearchConfig(), quotes, now_ns=NOW_NS)
    if verify:  # stand-in for a human running /parse-rules on every parsed row
        mm = read_market_map(home.market_map)
        for r in mm.markets:
            r.verified = bool(r.race_id)
        home.market_map.write_text(to_json(mm))


def test_end_to_end_targets_offline(home, quotes) -> None:
    _prepare_home(home, quotes, verify=True)
    run = run_targets(home, home.load_config(), quotes, now_ns=NOW_NS, n_draws=20_000, seed=3)
    assert run.path is not None
    assert run.path.exists()
    tf = read_current(home.targets)
    assert tf is not None
    assert run.path.name.endswith(f"-{tf.content_sha256[:8]}.json")
    assert tf.tournament_slug == "midterm-elections"
    assert {i.name for i in tf.inputs} >= {"reference_mids", "market_map", "cup_prices", "sim_params"}
    fair = {f.exchange_id: f for f in tf.fair}
    assert fair["1381"].p_yes == pytest.approx(0.86, abs=0.03)  # NH D
    assert fair["1205"].source == "sim"  # NC has no venue quote in the fixture
    assert tf.positions, run.notes
    assert all(p.rationale for p in tf.positions)
    assert tf.tourney.field_size == 2000
    assert tf.tourney.scenario
    assert tf.tourney.threshold_t_p90 >= tf.tourney.threshold_t_p50 >= 1.0
    # Whichever engine the facade selected (the C++ susq_models when installed, else numpy) must be the one
    # recorded in the provenance; the test must not assume which one this machine has.
    engine = json.loads((home.research_state / "calibration.json").read_text())["engine"]
    assert engine in {"numpy", "susq_models"}
    assert "field-model(" in tf.notes
    assert "early book frac 0.20 until 2026-10-21T16:00Z" in tf.notes  # NOW_NS is Sept 29
    assert (home.research_state / "sim_probs.json").exists()
    late = run_targets(
        home, home.load_config(), quotes, now_ns=NOW_NS + 30 * 86_400 * 10**9, n_draws=20_000, seed=3
    )
    assert "full book frac 0.60" in late.notes


def test_unverified_map_yields_fair_values_but_no_positions(home, quotes) -> None:
    _prepare_home(home, quotes, verify=False)
    run = run_targets(home, home.load_config(), quotes, now_ns=NOW_NS, n_draws=10_000, seed=3)
    tf = read_current(home.targets)
    assert tf is not None
    assert tf.fair
    assert not tf.positions
    assert "0/" in run.notes  # no tradable legs


def test_research_context_fair_and_change_detection(home, quotes) -> None:
    _prepare_home(home, quotes, verify=True)
    ctx = ResearchContext(home, home.load_config(), clock_ns=lambda: NOW_NS)
    assert ctx.targets_if_changed().startswith("no baseline")
    assert "fair values" in ctx.fair()
    assert home.fair_live.exists()
    assert "1 flags" in ctx.fair()  # MI R: Cup 0.80 vs fair ~0.30
    ctx._quotes = quotes
    ctx._targets_basis = ({k: v for k, v in ctx._basis(quotes).items()}, None)
    assert ctx.targets_if_changed().startswith("no material change")


def test_optimizer_section_is_read_from_toml(home, tmp_path) -> None:
    cfg = home.load_config()  # the repo's config/susq.toml
    opt = cfg.optimizer
    assert (opt.book_frac, opt.early_book_frac, opt.max_legs, opt.max_leg_frac) == (0.60, 0.20, 12, 0.12)
    assert opt.early_until_utc == datetime(2026, 10, 21, 16, tzinfo=UTC)
    custom = tmp_path / "custom.toml"
    custom.write_text(
        "[optimizer]\nbook_frac = 0.5\nearly_book_frac = 0.2\nearly_until_utc = 2026-10-15T12:00:00Z\n"
        "max_legs = 8\nmax_leg_frac = 0.1\n"
    )
    opt = ResearchConfig.from_toml(custom).optimizer
    assert (opt.book_frac, opt.early_book_frac, opt.max_legs, opt.max_leg_frac) == (0.5, 0.2, 8, 0.1)
    assert opt.early_until_utc == datetime(2026, 10, 15, 12, tzinfo=UTC)  # a TOML datetime literal works too
    assert opt.min_lift == ResearchConfig().optimizer.min_lift  # keys not in the table keep their defaults
    assert ResearchConfig.from_mapping({}).optimizer == ResearchConfig().optimizer


def test_daily_job_times_respect_pacific_dst() -> None:
    job = Job("report", lambda: "", daily_at=time(7, 0))
    before = job.next_run(datetime(2026, 10, 30, 15, tzinfo=UTC))  # 08:00 PDT
    assert before == datetime(2026, 10, 31, 14, tzinfo=UTC)  # 07:00 PDT = 14:00Z
    after = job.next_run(datetime(2026, 10, 31, 15, tzinfo=UTC))
    assert after == datetime(2026, 11, 1, 15, tzinfo=UTC)  # DST over: 07:00 PST = 15:00Z


def test_failing_job_is_contained_and_recorded(tmp_path) -> None:
    def boom() -> str:
        raise RuntimeError("feed down")

    job = Job("collect", boom, interval_s=60.0)
    sched = Scheduler([job], tmp_path / "health.json")
    assert asyncio.run(sched.run_once(job)) is False
    assert asyncio.run(sched.run_once(job)) is False
    h = sched.health["collect"]
    assert (h.runs, h.failures, h.consecutive_failures) == (2, 2, 2)
    assert "feed down" in h.last_error
    assert sched.retry_delay_s(job) == 60.0  # never faster than the job's own interval
    daily = Job("targets", boom, daily_at=time(6, 30))
    asyncio.run(sched.run_once(daily))
    assert 5.0 <= sched.retry_delay_s(daily) <= 600.0  # a failed daily job retries within minutes
    doc = json.loads((tmp_path / "health.json").read_text())
    assert doc["jobs"]["collect"]["failures"] == 2


def test_scheduler_loop_runs_jobs_and_stops(tmp_path) -> None:
    calls: list[str] = []
    jobs = [
        Job("fast", lambda: calls.append("fast") or "ok", interval_s=0.01),
        Job("broken", lambda: 1 / 0, interval_s=0.01),
        Job("daily", lambda: calls.append("daily") or "ok", daily_at=time(3, 0), run_at_start=False),
    ]
    sched = Scheduler(jobs, tmp_path / "h.json")

    async def main() -> None:
        stop = asyncio.Event()
        task = asyncio.create_task(sched.run(stop))
        await asyncio.sleep(0.2)
        stop.set()
        await asyncio.wait_for(task, 2)

    asyncio.run(main())
    assert calls.count("fast") >= 3
    assert "daily" not in calls
    assert sched.health["broken"].failures >= 1
    assert sched.health["daily"].next_run_utc


def test_default_jobs_cover_the_cadence(home) -> None:
    ctx = ResearchContext(home, home.load_config())
    jobs = {j.name: j for j in default_jobs(ctx)}
    assert jobs["collect"].interval_s == 60
    assert jobs["fair"].interval_s == 15
    assert jobs["telemetry"].interval_s == 3600
    assert jobs["report"].daily_at == time(7, 0)
    assert jobs["targets"].daily_at == time(6, 30)
    assert timedelta(seconds=jobs["targets-if-changed"].interval_s) == timedelta(hours=1)
