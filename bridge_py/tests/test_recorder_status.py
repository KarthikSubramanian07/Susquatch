"""Recorder Parquet schemas and the status snapshot."""

from __future__ import annotations

import pyarrow.parquet as pq
from susq.contracts import from_json, ops_pb2


async def test_recorder_writes_the_documented_schemas(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    rec = live_bridge.recorder
    prices_path = await rec.record_prices(live_bridge.market)
    lb_path = await rec.record_leaderboard(live_bridge.gateway)
    markets_path = await rec.snapshot_markets(live_bridge.gateway, live_bridge.market)
    assert prices_path is not None
    assert "prices/date=2026-10-01/part-" in str(prices_path)
    schema = pq.read_schema(prices_path)
    assert {f.name: str(f.type) for f in schema} == {
        "ts": "timestamp[us, tz=UTC]",
        "exchange_id": "large_string",
        "market_id": "large_string",
        "latest_price": "double",
        "best_bid": "double",
        "best_ask": "double",
        "spread": "double",
    }
    assert pq.read_metadata(prices_path).row_group(0).column(0).compression == "ZSTD"
    assert pq.read_table(prices_path).num_rows == 19
    assert lb_path is not None
    lb = pq.read_table(lb_path)
    assert {f.name: str(f.type) for f in lb.schema} == {
        "ts": "timestamp[us, tz=UTC]",
        "rank": "int32",
        "profile_id": "large_string",
        "username": "large_string",
        "pnl": "double",
        "trades_count": "int32",
        "volume": "double",
        "win_rate": "double",
        "roi": "double",
        "total": "int32",
        "my_rank": "int32",
    }
    assert lb.num_rows == lb.column("total")[0].as_py()
    raw = markets_path.read_text()
    assert '"nodes"' in raw
    assert "Senate" in raw


async def test_status_snapshot_roundtrips(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    snap = live_bridge.status.write(live_bridge)
    parsed = from_json(ops_pb2.StatusSnapshot, live_bridge.config.status_path.read_text())
    assert parsed == snap
    assert parsed.mode == "live"
    assert parsed.stage == "forage"
    assert parsed.trader == "bridge"
    assert {h.name for h in parsed.health} >= {"platform", "realtime", "fair", "lease", "risk"}


async def test_status_cash_is_the_last_portfolio_read(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    """EXP-015: cash is account value minus holdings from the last read, not start-up ``myBalance``."""
    risk = live_bridge.gateway.risk
    assert risk.cash is not None  # start() reconciled
    live_bridge.tournament.my_balance = 27_254.41  # the stale start-up figure
    risk.cash = 17_305.0  # as a later portfolio read would set it
    assert live_bridge.status.build(live_bridge).cash == 17_305.0
    await live_bridge.oms.reconcile()
    pnl = await live_bridge.client.get_pnl(live_bridge.tournament.slug, period="all")
    assert pnl.total_holdings_value is not None
    expected = pnl.total_account_value - pnl.total_holdings_value
    assert live_bridge.status.build(live_bridge).cash == expected


async def test_status_cash_falls_back_to_my_balance_before_the_first_read(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("live", start=False)
    try:
        assert bridge.gateway.risk.cash is None
        bridge.tournament.my_balance = 27_254.41
        assert bridge.status.build(bridge).cash == 27_254.41
    finally:
        await bridge.close()
