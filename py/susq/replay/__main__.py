"""`python -m susq.replay`: build replays and priors, stream them, run drills, fetch history.

Examples (from the repo root)::

    uv run python -m susq.replay build py/susq/replay/specs/sample_2022.toml --out data/replay/s.jsonl
    uv run python -m susq.replay prior py/susq/replay/specs/sample_2022.toml --out data/replay/s.prior.json
    uv run python -m susq.replay drill py/susq/replay/specs/sample_2022.toml --chaos-drop 1800:600
    uv run python -m susq.replay drill-compare py/susq/replay/specs/sample_2022.toml
    uv run python -m susq.replay publish data/replay/s.jsonl --nats nats://127.0.0.1:4222 --speed 120
    uv run python -m susq.replay download 2022 GA NH ME --out data/replay/county_results_2022.csv
    uv run python -m susq.replay download 2024 --market-map state/market_map.json \
        --out data/replay/county_results_2024.csv
    uv run python -m susq.replay download 2022 CA --house-only --out data/replay/house_results_2022.csv
    uv run python -m susq.replay spec2026 --market-map state/market_map.json --fair state/fair_live.json \
        --history data/replay/county_results_2022.csv --history data/replay/county_results_2024.csv \
        --history data/replay/house_results_2022.csv --history data/replay/house_results_2024.csv \
        --out py/susq/replay/specs/general_2026.toml
"""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime
from pathlib import Path
from typing import Annotated

import typer

from susq.contracts import to_json
from susq.replay.backends import select_backend
from susq.replay.closing import DEFAULT_PATH as CLOSING_PATH
from susq.replay.closing import REPO_ROOT
from susq.replay.count_order import DEFAULT_PATH as COUNT_ORDER_PATH
from susq.replay.count_order import CountOrderTable
from susq.replay.drill import (
    ExecutorConfig,
    compare_count_order,
    render_comparison,
    render_report,
    simulate_executor,
    write_report,
)
from susq.replay.records import read_replay, write_replay
from susq.replay.runner import ChaosConfig, replay_in_process, replay_to_nats
from susq.replay.spec import load_spec, materialize, materialize_prior

app = typer.Typer(no_args_is_help=True, help="Synthetic election-night replays, priors and drills.")

OutPath = Annotated[Path, typer.Option("--out", help="output file")]
Drop = Annotated[str | None, typer.Option(help="feed drop window, start:duration seconds")]
Malformed = Annotated[int, typer.Option(help="inject a malformed payload every N deliveries (0 = off)")]
Kill = Annotated[str | None, typer.Option(help="publisher kill, start:restart_after seconds")]


def _window(s: str | None) -> tuple[float, float] | None:
    """Parse "start:duration" (seconds)."""
    if not s:
        return None
    a, b = s.split(":", 1)
    return float(a), float(b)


def _chaos(drop: str | None, malformed_every: int, kill: str | None) -> ChaosConfig:
    d = _window(drop)
    k = _window(kill)
    return ChaosConfig(
        drop_start_s=d[0] if d else None,
        drop_duration_s=d[1] if d else 0.0,
        malformed_every=malformed_every,
        kill_at_s=k[0] if k else None,
        restart_after_s=k[1] if k else 60.0,
    )


@app.command()
def build(spec: Path, out: OutPath = Path("data/replay/replay.jsonl")) -> None:
    """Build a synthetic replay (JSONL + meta) from a spec."""
    batches, meta, _ = materialize(load_spec(spec))
    n = write_replay(out, batches, meta)
    typer.echo(f"wrote {n} batches to {out}")


@app.command()
def prior(spec: Path, out: OutPath = Path("data/replay/prior.json")) -> None:
    """Build the NeedlePrior (proto3 JSON) for a spec."""
    p = materialize_prior(load_spec(spec))
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(to_json(p))
    typer.echo(f"wrote {len(p.races)} races / {len(p.counties)} counties to {out}")


@app.command()
def publish(
    replay: Path,
    nats_url: Annotated[str, typer.Option("--nats")] = "nats://127.0.0.1:4222",
    speed: Annotated[float, typer.Option(help="replay seconds per wall second")] = 60.0,
    chaos_drop: Drop = None,
    chaos_malformed_every: Malformed = 0,
    chaos_kill: Kill = None,
) -> None:
    """Stream a replay to NATS `enr.county_results` (timestamps re-anchored to now)."""
    _, batches = read_replay(replay)
    chaos = _chaos(chaos_drop, chaos_malformed_every, chaos_kill)
    stats = asyncio.run(replay_to_nats(batches, nats_url, speed=speed, chaos=chaos))
    typer.echo(
        f"published {stats.published} (+{stats.malformed} malformed, {stats.reconnects} restarts) "
        f"in {stats.wall_s:.1f}s"
    )


@app.command()
def drill(
    spec: Path,
    backend: Annotated[str, typer.Option(help="cpp (falls back to reference) or reference")] = "cpp",
    speed: Annotated[float, typer.Option(help="replay seconds per wall second; 0 = max")] = 0.0,
    market_delay_s: Annotated[float, typer.Option(help="how far the Cup book lags our posterior")] = 600.0,
    chaos_drop: Drop = None,
    chaos_malformed_every: Malformed = 0,
    chaos_kill: Kill = None,
    out_dir: Annotated[Path, typer.Option()] = REPO_ROOT / "reports" / "drills",
) -> None:
    """Replay a spec through the needle and a simulated executor; write reports/drills/<date>.md."""
    batches, meta, p = materialize(load_spec(spec))
    needle, note = select_backend(backend)
    needle.load_prior(p)
    chaos = _chaos(chaos_drop, chaos_malformed_every, chaos_kill)
    record = replay_in_process(batches, needle, speed=speed, chaos=chaos)
    cfg = ExecutorConfig(market_delay_s=market_delay_s)
    result = simulate_executor(record, p, meta.races, cfg)
    text = render_report(result, title=spec.stem, replay=str(spec), backend_note=note, cfg=cfg)
    path = write_report(text, out_dir)
    typer.echo(
        f"{path}: {len(result.trades)} trades, P&L {result.pnl:+.1f}, false triggers {result.false_triggers}"
    )


@app.command("drill-compare")
def drill_compare(
    spec: Path,
    backend: Annotated[str, typer.Option(help="cpp (falls back to reference) or reference")] = "cpp",
    market_delay_s: Annotated[float, typer.Option(help="how far the Cup book lags our posterior")] = 600.0,
    table: Annotated[Path, typer.Option(help="count-order table")] = COUNT_ORDER_PATH,
    out_dir: Annotated[Path, typer.Option()] = REPO_ROOT / "reports" / "drills",
) -> None:
    """EXP-006: replay a spec once, run the executor with the count-order gate off and on, and write
    reports/drills/<date>-count-order.md."""
    batches, meta, p = materialize(load_spec(spec))
    needle, note = select_backend(backend)
    needle.load_prior(p)
    record = replay_in_process(batches, needle, speed=0.0, chaos=ChaosConfig())
    cfg = ExecutorConfig(market_delay_s=market_delay_s)
    co = CountOrderTable.load(table)
    cmp = compare_count_order(record, p, meta.races, cfg, co)
    text = render_comparison(cmp, title=spec.stem, replay=str(spec), backend_note=note, cfg=cfg, table=co)
    path = write_report(text, out_dir, date=datetime.now(UTC).strftime("%Y-%m-%d") + "-count-order")
    typer.echo(
        f"{path}: gate off {len(cmp.off.trades)} trades P&L {cmp.off.pnl:+.1f}; "
        f"gate on {len(cmp.on.trades)} trades P&L {cmp.on.pnl:+.1f}"
    )


@app.command()
def download(
    year: int,
    states: Annotated[list[str] | None, typer.Argument(help="USPS codes; omit with --market-map")] = None,
    out: OutPath = Path("data/replay/county_results.csv"),
    market_map: Annotated[
        Path | None,
        typer.Option(help="state/market_map.json: add every state with a Senate or Governor market"),
    ] = None,
    house: Annotated[bool, typer.Option(help="also keep US House rows, by county x district")] = False,
    house_only: Annotated[
        bool, typer.Option(help="keep only US House rows (a House history file for spec2026)")
    ] = False,
    refresh: Annotated[bool, typer.Option(help="ignore the per-state cache under data/replay/medsl")] = False,
) -> None:
    """Download MEDSL precinct returns for `states` and aggregate them to county CSV.

    Each state is cached under `<out dir>/medsl/<year>/<ST>[.house|.with-house].csv`; failures are
    reported, not fatal.
    """
    from susq.replay.download import download as fetch
    from susq.replay.spec2026 import cup_states

    wanted = sorted({s.upper() for s in states or []} | (cup_states(market_map) if market_map else set()))
    if not wanted:
        raise typer.BadParameter("give states or --market-map")
    res = fetch(year, wanted, out, house=house, house_only=house_only, refresh=refresh)
    typer.echo(f"wrote {res.out}: {len(res.done)} states ({len(res.cached)} cached)")
    if res.empty:
        typer.echo(f"  no Senate/Governor/President rows: {' '.join(res.empty)}")
    for st, err in res.failed.items():
        typer.echo(f"  FAILED {st}: {err}", err=True)
    if res.failed:
        raise typer.Exit(code=1)


@app.command()
def spec2026(
    market_map: Annotated[Path, typer.Option(help="state/market_map.json")] = Path("state/market_map.json"),
    fair: Annotated[Path, typer.Option(help="state/fair_live.json (TargetFile)")] = Path(
        "state/fair_live.json"
    ),
    history: Annotated[
        list[Path] | None, typer.Option(help="county results CSV from `download` (repeat for 2022 and 2024)")
    ] = None,
    poll_closing: Annotated[Path, typer.Option(help="poll closing table")] = CLOSING_PATH,
    out: OutPath = Path("py/susq/replay/specs/general_2026.toml"),
) -> None:
    """Write the 2026 needle spec: every Cup Senate and Governor race and California House seat,
    margins from the fair file."""
    from susq.replay.spec2026 import build_spec, render_toml

    paths = history or [
        Path("data/replay/county_results_2022.csv"),
        Path("data/replay/county_results_2024.csv"),
        # California House history (`download <year> CA --house-only`), when it has been fetched.
        *(p for y in (2022, 2024) if (p := Path(f"data/replay/house_results_{y}.csv")).exists()),
    ]
    missing = [p for p in paths if not p.exists()]
    if missing:
        raise typer.BadParameter(f"history not found: {', '.join(map(str, missing))} (run `download` first)")
    spec = build_spec(market_map, fair, paths, poll_closing)
    cmd = (
        f"python -m susq.replay spec2026 --market-map {market_map} --fair {fair} "
        + " ".join(f"--history {p}" for p in paths)
        + f" --out {out}"
    )
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(render_toml(spec, generated_on=datetime.now(UTC).date(), command=cmd))
    typer.echo(f"wrote {out}: {len(spec.races)} races, {len(spec.uncovered)} uncovered")
    for u in spec.uncovered:
        typer.echo(f"  uncovered {u.race_id}: {u.reason}")


if __name__ == "__main__":
    app()
