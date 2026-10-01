"""``susq-bridge``: the Oct-1 bridge's command line.

Every command loads ``config/susq.toml`` + ``.env`` from ``$SUSQ_HOME`` (default: the current directory).
Paper mode is the default; live needs ``SUSQ_MODE=live`` AND ``ops/GO_LIVE`` containing "go live".
"""

from __future__ import annotations

import asyncio
import logging
import os
import socket
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime
from pathlib import Path
from typing import Annotated, Any

import orjson
import typer
from susq.contracts import from_json, ops_pb2

from susq_bridge.config import BridgeConfig, load_config
from susq_bridge.first_trade import first_trade
from susq_bridge.lease import Lease
from susq_bridge.measure_day1 import Day1Report, collateral_confirmed, measure_day1, write_report
from susq_bridge.runtime import Bridge, BridgeError, Loops

app = typer.Typer(no_args_is_help=True, help="Susquatch Oct-1 Python bridge.")
lease_app = typer.Typer(no_args_is_help=True, help="Inspect or release the single-trader lease.")
app.add_typer(lease_app, name="lease")

HomeOpt = Annotated[Path | None, typer.Option("--home", help="SUSQ_HOME (repo checkout); default: cwd")]


@app.callback()
def _root(verbose: Annotated[bool, typer.Option("--verbose", "-v")] = False) -> None:
    """Group callback so sub-commands stay sub-commands."""
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    logging.getLogger("httpx").setLevel(logging.DEBUG if verbose else logging.WARNING)


def _config(home: Path | None) -> BridgeConfig:
    return load_config(home)


async def _with_bridge(
    config: BridgeConfig, fn: Callable[[Bridge], Awaitable[Any]], *, start: bool = True
) -> Any:
    bridge = await Bridge.open(config)
    try:
        if start:
            await bridge.start()
        return await fn(bridge)
    finally:
        if bridge.lease.held():
            bridge.lease.release()
        await bridge.close()


def _fail(message: str) -> None:
    typer.secho(message, fg=typer.colors.RED, err=True)
    raise typer.Exit(code=1)


@app.command()
def version() -> None:
    """Print the bridge version."""
    from susq_bridge import __version__

    typer.echo(__version__)


@app.command("measure-day1")
def measure_day1_cmd(
    home: HomeOpt = None,
    rate_probe: Annotated[bool, typer.Option(help="Ramp GET /account to find the rate limit")] = True,
) -> None:
    """Probe the platform and write reports/DAY1.md + reports/day1.json (write probes are live-only)."""
    config = _config(home)

    async def run() -> None:
        bridge: Bridge | None = None
        if config.has_api_key:
            try:
                bridge = await Bridge.open(config)
                await bridge.start()
            except BridgeError as exc:
                typer.secho(f"bridge unavailable: {exc}", fg=typer.colors.YELLOW, err=True)
                bridge = None
        try:
            report = await measure_day1(config, bridge=bridge, rate_probe=rate_probe)
        finally:
            if bridge is not None:
                if bridge.lease.held():
                    bridge.lease.release()
                await bridge.close()
        md, _ = write_report(report, config.reports_dir)
        typer.echo(md.read_text())

    asyncio.run(run())


@app.command("first-trade")
def first_trade_cmd(home: HomeOpt = None) -> None:
    """Buy exactly one YES share near the mid in the most liquid market (live mode only)."""
    config = _config(home)
    try:
        result = asyncio.run(_with_bridge(config, first_trade))
    except BridgeError as exc:
        _fail(str(exc))
        return
    typer.echo(
        f"first trade: {result.status} ({result.reason}) exchange={result.exchange_id} price={result.price}"
    )
    if result.status == "failed":
        raise typer.Exit(code=1)


def _loops(racer: bool, sniper: bool, recorder: bool, realtime: bool) -> Loops:
    return Loops(racer=racer, sniper=sniper, recorder=recorder, realtime=realtime)


@app.command()
def run(
    home: HomeOpt = None,
    racer: Annotated[bool, typer.Option(help="Constraints racer")] = True,
    sniper: Annotated[bool, typer.Option(help="Stale-order sniper")] = True,
    recorder: Annotated[bool, typer.Option(help="Market-data recorder")] = True,
    realtime: Annotated[bool, typer.Option(help="Supabase Realtime (polling fallback otherwise)")] = True,
) -> None:
    """Run racer + sniper + recorder + reconcile + status until interrupted."""
    config = _config(home)

    async def go(bridge: Bridge) -> None:
        _apply_collateral(bridge, config, None)
        await bridge.run(_loops(racer, sniper, recorder, realtime))

    try:
        asyncio.run(_with_bridge(config, go))
    except BridgeError as exc:
        _fail(str(exc))


def _apply_collateral(bridge: Bridge, config: BridgeConfig, report: Day1Report | None) -> None:
    """``use_collateral_buying_power = "auto"``: switch collateral sizing on once day 1 confirmed it."""
    if not config.risk.collateral_auto:
        return
    on = collateral_confirmed(report, config.state_dir)
    bridge.gateway.risk.collateral_confirmed = on
    typer.echo(f"collateral buying power (auto): {'on, the engine matched the model' if on else 'off'}")


@app.command("open")
def open_cmd(
    home: HomeOpt = None,
    key_poll_s: Annotated[float, typer.Option(help="How often to look for SUSQ_API_KEY in .env")] = 30.0,
) -> None:
    """Wait for the API key and for calendar.open_utc, then measure-day1 -> first-trade (live) -> run."""
    config = _config(home)
    while not config.has_api_key:
        typer.echo(f"waiting for SUSQ_API_KEY in {config.home / '.env'} ...")
        asyncio.run(asyncio.sleep(key_poll_s))
        config = _config(home)
    wait = (config.calendar.open_utc - datetime.now(UTC)).total_seconds()
    if wait > 0:
        typer.echo(f"sleeping {wait / 3600:.2f} h until {config.calendar.open_utc.isoformat()}")
        asyncio.run(asyncio.sleep(wait))

    async def sequence(bridge: Bridge) -> None:
        report = await measure_day1(config, bridge=bridge)
        write_report(report, config.reports_dir)
        _apply_collateral(bridge, config, report)
        if bridge.mode == "live":
            result = await first_trade(bridge)
            typer.echo(f"first trade: {result.status} ({result.reason})")
        await bridge.run(Loops())

    try:
        asyncio.run(_with_bridge(config, sequence))
    except BridgeError as exc:
        _fail(str(exc))


@app.command("cancel-all")
def cancel_all_cmd(
    home: HomeOpt = None,
    exchange: Annotated[str | None, typer.Option(help="Only this exchange id")] = None,
) -> None:
    """Cancel every open order in the tournament (needs the lease)."""
    config = _config(home)

    async def go(bridge: Bridge) -> bool:
        return await bridge.gateway.cancel_all("cli", exchange_id=exchange)

    try:
        ok = asyncio.run(_with_bridge(config, go))
    except BridgeError as exc:
        _fail(str(exc))
        return
    typer.echo("all cancelled" if ok else "some orders may still be open (see the journal)")
    if not ok:
        raise typer.Exit(code=1)


@app.command()
def status(
    home: HomeOpt = None,
    refresh: Annotated[
        bool, typer.Option(help="Reconcile from the platform and rewrite status.json")
    ] = False,
) -> None:
    """Print state/status.json (optionally refreshing it first)."""
    config = _config(home)
    if refresh:

        async def go(bridge: Bridge) -> None:
            bridge.status.write(bridge)

        try:
            asyncio.run(_with_bridge(config, go))
        except BridgeError as exc:
            _fail(str(exc))
    if not config.status_path.exists():
        _fail(f"no status yet at {config.status_path}")
    snap = from_json(ops_pb2.StatusSnapshot, config.status_path.read_text())
    typer.echo(
        f"mode={snap.mode} stage={snap.stage} trader={snap.trader or '-'} equity={snap.equity:.2f} "
        f"killed={snap.killed} rank={snap.rank or '-'} field={snap.field_size}"
    )
    for item in snap.health:
        typer.echo(f"  {item.name:<9} {item.state:<9} {item.detail}")
    for need in snap.action_needed:
        typer.secho(f"  ACTION NEEDED: {need}", fg=typer.colors.YELLOW)


@app.command()
def record(
    home: HomeOpt = None,
    loop: Annotated[bool, typer.Option(help="Keep recording on the telemetry intervals")] = False,
) -> None:
    """Recorder only: markets + node trees, prices and the leaderboard (useful before the open)."""
    config = _config(home)

    async def go(bridge: Bridge) -> None:
        path = await bridge.recorder.snapshot_markets(bridge.gateway, bridge.market)
        typer.echo(f"markets -> {path}")
        await bridge.recorder.record_prices(bridge.market)
        await bridge.recorder.record_leaderboard(bridge.gateway)
        if loop:
            await bridge.run(Loops(racer=False, sniper=False, realtime=False))

    try:
        asyncio.run(_with_bridge(config, go))
    except BridgeError as exc:
        _fail(str(exc))


@lease_app.command("show")
def lease_show(home: HomeOpt = None) -> None:
    """Print the current lease."""
    config = _config(home)
    current = Lease(config.lease_path, "cli", ttl_s=config.lease_ttl_s).read()
    if current is None:
        typer.echo("no lease")
        return
    now_ns = datetime.now(UTC).timestamp() * 1e9
    state = "active" if current.expires_at_ns > now_ns else "expired"
    typer.echo(
        orjson.dumps(
            {
                "holder": current.holder,
                "host": current.host,
                "pid": current.pid,
                "epoch": current.epoch,
                "state": state,
                "expiresInS": round((current.expires_at_ns - now_ns) / 1e9, 1),
            },
            option=orjson.OPT_INDENT_2,
        ).decode()
    )


@lease_app.command("release")
def lease_release(
    home: HomeOpt = None,
    force: Annotated[bool, typer.Option(help="Release even if the holder process looks alive")] = False,
) -> None:
    """Expire the lease so another process can take over (refuses while the holder is alive)."""
    config = _config(home)
    current = Lease(config.lease_path, "cli", ttl_s=config.lease_ttl_s).read()
    if current is None:
        typer.echo("no lease")
        return
    here = current.host == socket.gethostname()
    if not force and (not here or _alive(current.pid)):
        where = f"pid {current.pid} is alive on this host" if here else f"it lives on {current.host}"
        _fail(f"refusing to release {current.holder}'s lease: {where}; stop it first or use --force")
    holder = Lease(
        config.lease_path, current.holder, ttl_s=config.lease_ttl_s, host=current.host, pid=current.pid
    )
    holder.release()
    typer.echo(f"released lease held by {current.holder} (epoch {current.epoch})")


def _alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True
