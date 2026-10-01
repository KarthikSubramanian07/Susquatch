"""measure_day1, first_trade and the offline CLI commands."""

from __future__ import annotations

import os
from pathlib import Path

import orjson
from susq.mock import MockConfig
from susq.mock.testing import KEY, serve_mock
from susq_bridge.cli import _apply_collateral, app
from susq_bridge.config import load_config
from susq_bridge.first_trade import first_trade
from susq_bridge.lease import Lease
from susq_bridge.measure_day1 import (
    Day1Report,
    collateral_confirmed,
    decide,
    load_report,
    measure_day1,
    probe_rate_limit,
    write_report,
)
from susq_bridge.runtime import Bridge
from typer.testing import CliRunner


def by_name(report) -> dict:  # type: ignore[no-untyped-def,type-arg]
    return {s.name: s for s in report.sections}


async def test_measure_day1_live(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("live")
    try:
        report = await measure_day1(
            harness.config,
            bridge=bridge,
            transport=harness.mock.transport,
            rate_steps=(2, 4),
            rate_step_s=0.5,
            realtime_probe=False,
        )
        s = by_name(report)
        assert s["hosts"].status == "ok"
        assert s["tournament"].data["marketCount"] == 19
        assert s["tick"].status == "ok", s["tick"].summary
        assert s["tick"].data["code"] == "VALIDATION_ERROR"
        assert s["no_mechanics"].status == "ok"
        assert s["no_mechanics"].data["canonical"] == ["buy", "no", 0.05, 1]
        assert s["no_mechanics"].data["listedOpen"] is True
        assert s["no_mechanics"].data["secondCancel"] == "409 CONFLICT"
        assert s["rate_limit"].data == {"firstLimitedAtRps": None, "retryAfterS": None, "sustainedRps": 4}
        assert s["census"].data["exchanges"] == 19
        assert s["census"].data["twoSidedFrac"] == 1.0
        assert s["leaderboard"].status == "ok"
        assert (
            s["constraint_prices"].data["matchLastTrade"] == s["constraint_prices"].data["observedCompared"]
        )
        assert s["fees"].status == "pending"
        assert s["market_vs_limit"].status == "skipped"
        assert any(r.startswith("Row 7") for r in report.recommendations)
        assert any(r.startswith(("Row 1", "Row 2", "Rows 1-2")) for r in report.recommendations)
        col = s["collateral"]
        assert col.status == "ok", col.summary
        assert col.data["source"] == "probe"
        assert (col.data["filled"], col.data["allNonNull"]) == (2, 2)
        assert col.data["savings"] > 0  # the second NO leg was backed by the pair's floor of 1
        assert col.data["mismatches"] == []
        assert col.data["before"] == []
        (component,) = col.data["after"]
        assert component["guaranteedPayoutFloor"] == 1
        assert any("set use_collateral_buying_power = true" in r for r in report.recommendations)
        assert (
            await harness.mock.client.list_open_orders(harness.mock.tid) == []
        )  # probes left nothing resting
        # The only position left is the collateral probe: 1 NO on each leg of one pair (pays >= 1).
        positions = (await harness.mock.client.get_positions("midterm-elections")).positions
        assert sorted(p.quantity for p in positions) == [-1, -1]
        assert {p.exchange_id for p in positions} == set(component["coveredExchangeIds"])
        md, js = write_report(report, harness.config.reports_dir)
        assert "Decision table" in md.read_text()
        assert load_report(harness.config.reports_dir).get("tick").status == "ok"  # type: ignore[union-attr]
        assert js.exists()
        # The repo config says "auto": the matched probe switches collateral sizing on at once.
        assert harness.config.risk.collateral_auto
        assert not bridge.gateway.risk.uses_collateral
        _apply_collateral(bridge, harness.config, report)
        assert bridge.gateway.risk.uses_collateral
        assert (harness.config.state_dir / "bridge" / "collateral_confirmed.json").exists()
    finally:
        await bridge.close()


async def test_collateral_mismatch_keeps_the_flag_off(harness) -> None:  # type: ignore[no-untyped-def]
    """An engine that returns `all: null` on linked fills must not earn the recommendation."""
    for acct in harness.mx.engine.state.accounts.values():
        acct.collateral.enabled = False  # the mock now behaves like an engine without ALL
    bridge = await harness.open_bridge("live")
    try:
        report = await measure_day1(
            harness.config,
            bridge=bridge,
            rate_probe=False,
            realtime_probe=False,
            transport=harness.mock.transport,
        )
        col = by_name(report)["collateral"]
        assert col.status == "failed"
        assert col.data["allNonNull"] == 0
        assert "came back null" in col.summary
        rec = next(r for r in report.recommendations if r.startswith("Collateral"))
        assert "keep [risk] use_collateral_buying_power = false" in rec
        assert all("= true" not in r for r in report.recommendations)
        _apply_collateral(bridge, harness.config, report)
        assert not bridge.gateway.risk.uses_collateral  # "auto" stays off on a mismatch
    finally:
        await bridge.close()


def test_collateral_recommendation_explains_a_model_mismatch() -> None:
    report = Day1Report(generated_at="now", mode="live")
    report.add(
        "collateral",
        "failed",
        "probe: engine differs from the model: 1302 collateralSavings: engine 0.0000 vs model 0.4500",
        matches=False,
    )
    rec = next(r for r in decide(report) if r.startswith("Collateral"))
    assert "keep [risk] use_collateral_buying_power = false" in rec
    assert "collateralSavings" in rec


async def test_rate_probe_stops_at_the_first_429(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.mock.post_admin("/_mock/faults", {"rate_limit_rps": 3, "retry_after_s": 2})
    result = await probe_rate_limit(harness.mock.client, steps=(2, 4, 6), seconds_per_step=0.5)
    assert result == {"firstLimitedAtRps": 6, "retryAfterS": 2.0, "sustainedRps": 4}


async def test_measure_day1_paper_skips_writes(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("paper")
    try:
        report = await measure_day1(harness.config, bridge=bridge, rate_probe=False, realtime_probe=False)
        s = by_name(report)
        assert s["tick"].status == "skipped"
        assert s["no_mechanics"].status == "skipped"
        assert s["census"].status == "ok"
        assert s["collateral"].status == "skipped"
        assert "paper mode" in s["collateral"].summary
        assert any("keep [risk] use_collateral_buying_power = false" in r for r in report.recommendations)
    finally:
        await bridge.close()


async def test_measure_day1_without_a_key(home: Path) -> None:
    config = load_config(home, env={})
    report = await measure_day1(config, bridge=None)
    assert {s.status for s in report.sections} == {"skipped"}
    assert report.action_needed


async def test_first_trade_buys_one_share_and_records_fees(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("live")
    try:
        result = await first_trade(bridge, wait_s=0, poll_s=0)
        assert result.status == "filled"
        positions = (await harness.mock.client.get_positions("midterm-elections")).positions
        assert [(p.exchange_id, p.quantity) for p in positions] == [(result.exchange_id, 1)]
        obs = orjson.loads((harness.config.reports_dir / "first_trade.json").read_bytes())
        assert obs["quantity"] == 1
        report = load_report(harness.config.reports_dir)
        assert report is not None
        assert report.get("fees").data["impliedFee"] == 0  # type: ignore[union-attr]
        assert report.get("valuation").data["basis"] == "last trade"  # type: ignore[union-attr]
        again = await first_trade(bridge, wait_s=0, poll_s=0)
        assert again.status == "skipped"
        lb = await harness.mock.client.get_leaderboard("midterm-elections")
        assert lb.my_rank is not None
    finally:
        await bridge.close()


async def test_first_trade_is_live_only(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("paper")
    try:
        assert (await first_trade(bridge)).status == "skipped"
    finally:
        await bridge.close()


# ── CLI (offline commands) ──────────────────────────────────────────────────────────────────


def test_cli_version_and_empty_lease(home: Path) -> None:
    runner = CliRunner()
    assert runner.invoke(app, ["version"]).exit_code == 0
    res = runner.invoke(app, ["lease", "show", "--home", str(home)])
    assert res.exit_code == 0
    assert "no lease" in res.output


def test_cli_lease_release_refuses_a_live_holder(home: Path) -> None:
    config = load_config(home, env={})
    holder = Lease(config.lease_path, "bridge", ttl_s=30, pid=os.getpid())
    assert holder.acquire()
    runner = CliRunner()
    res = runner.invoke(app, ["lease", "release", "--home", str(home)])
    assert res.exit_code == 1
    assert holder.held()
    res = runner.invoke(app, ["lease", "release", "--home", str(home), "--force"])
    assert res.exit_code == 0
    assert not holder.held()
    shown = runner.invoke(app, ["lease", "show", "--home", str(home)])
    assert '"state": "expired"' in shown.output


def test_cli_status_without_a_snapshot_fails(home: Path) -> None:
    res = CliRunner().invoke(app, ["status", "--home", str(home)])
    assert res.exit_code == 1


def test_cli_measure_day1_without_a_key_writes_a_skipped_report(home: Path, monkeypatch) -> None:  # type: ignore[no-untyped-def]
    monkeypatch.delenv("SUSQ_API_KEY", raising=False)
    res = CliRunner().invoke(app, ["measure-day1", "--home", str(home)])
    assert res.exit_code == 0
    assert "ACTION NEEDED" in (home / "reports" / "DAY1.md").read_text()


async def test_realtime_probe_joins(home: Path) -> None:
    async with serve_mock(MockConfig(seed=7)) as live:
        config = load_config(home, env={"SUSQ_API_KEY": KEY, "SUSQ_BASE_URL": live.api})
        bridge = await Bridge.open(config)
        try:
            await bridge.start()
            report = await measure_day1(config, bridge=bridge, rate_probe=False)
            realtime = report.get("realtime")
            assert realtime is not None
            assert realtime.data["joined"] is True
        finally:
            await bridge.close()


# ── collateral "auto" decision ──────────────────────────────────────────────────────────────


def _with_collateral(status: str, **data: object) -> Day1Report:
    report = Day1Report(generated_at="2026-10-01T16:00:05Z", mode="live")
    report.add("collateral", status, "probe", **data)
    return report


def test_collateral_auto_confirms_persists_and_revokes(tmp_path: Path) -> None:
    record = tmp_path / "bridge" / "collateral_confirmed.json"
    assert collateral_confirmed(None, tmp_path) is False  # nothing measured yet
    assert collateral_confirmed(_with_collateral("ok", matches=True, filled=2), tmp_path) is True
    assert record.exists()
    # A restart after the open re-measures; a skipped probe keeps the earlier confirmation.
    assert collateral_confirmed(_with_collateral("skipped"), tmp_path) is True
    assert collateral_confirmed(None, tmp_path) is True
    # A measured mismatch switches it back off and forgets the confirmation.
    assert collateral_confirmed(_with_collateral("ok", matches=False), tmp_path) is False
    assert not record.exists()
    assert collateral_confirmed(_with_collateral("ok", matches=True), tmp_path) is True
    assert collateral_confirmed(_with_collateral("failed"), tmp_path) is False
