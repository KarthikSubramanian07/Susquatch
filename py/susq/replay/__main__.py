"""`python -m susq.replay`: build replays and priors, stream them, run drills, fetch history.

Examples (from the repo root)::

    uv run python -m susq.replay build py/susq/replay/specs/sample_2022.toml --out data/replay/s.jsonl
    uv run python -m susq.replay prior py/susq/replay/specs/sample_2022.toml --out data/replay/s.prior.json
    uv run python -m susq.replay drill py/susq/replay/specs/sample_2022.toml --chaos-drop 1800:600
    uv run python -m susq.replay publish data/replay/s.jsonl --nats nats://127.0.0.1:4222 --speed 120
    uv run python -m susq.replay download 2022 GA NH ME --out data/replay/county_results_2022.csv
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Annotated

import typer

from susq.contracts import to_json
from susq.replay.backends import select_backend
from susq.replay.closing import REPO_ROOT
from susq.replay.drill import ExecutorConfig, render_report, simulate_executor, write_report
from susq.replay.records import read_replay, write_replay
from susq.replay.runner import ChaosConfig, replay_in_process, replay_to_nats
from susq.replay.spec import load_spec, materialize

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
    _, _, p = materialize(load_spec(spec))
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


@app.command()
def download(year: int, states: list[str], out: OutPath = Path("data/replay/county_results.csv")) -> None:
    """Download MEDSL precinct returns for `states` and aggregate them to county CSV."""
    from susq.replay.download import download as fetch

    typer.echo(f"wrote {fetch(year, [s.upper() for s in states], out)}")


if __name__ == "__main__":
    app()
