"""Daily report metrics, rendering, proposal rules, review packets and the scrubber."""

from __future__ import annotations

import json
from datetime import UTC, date, datetime, timedelta

import polars as pl
import pytest
from google.protobuf import json_format
from susq.calibrate.targets import InputRef, build_target_file, write_targets
from susq.contracts import ops_pb2, targets_pb2
from susq.reports.daily import render
from susq.reports.job import run_report
from susq.reports.metrics import DailyMetrics, StrategyStats, collect
from susq.reports.packets import PacketLeakError, scrub_text, write_packet
from susq.reports.rules import propose
from susq.research_env import atomic_write_text

DAY = date(2026, 10, 5)
T = datetime(2026, 10, 5, 15, tzinfo=UTC)
T_NS = int(T.timestamp() * 1e9)
RACER_IDEM = "c5fe4b7e-7efc-55fb-82b5-1bd6356d699c"
SNIPER_IDEM = "0b8e1a55-4f38-5e8c-9c7c-2f0a1e6d4b21"


def _line(**kw) -> str:
    return json_format.MessageToJson(ops_pb2.JournalEntry(producer="bridge", **kw), indent=None) + "\n"


def _order_roundtrip(t: int, key: str, corr: str, request: dict, response: dict, latency_us: int) -> str:
    """A journaled POST /orders request and its 200 response."""
    common = {"method": "POST", "path": "/orders", "idempotency_key": key, "correlation_id": corr}
    return _line(ts_ns=t, direction="request", body_json=json.dumps(request), **common) + _line(
        ts_ns=t + 1,
        direction="response",
        status=200,
        latency_us=latency_us,
        body_json=json.dumps(response),
        **common,
    )


def _intent(intent_id: str, key: str, strategy: str, state: int, **kw) -> ops_pb2.OrderIntent:
    return ops_pb2.OrderIntent(
        intent_id=intent_id, idempotency_key=key, strategy=strategy, state=state, created_at_ns=T_NS, **kw
    )


def _seed_journal_and_intents(home) -> None:
    racer_req = {"exchangeId": "1381", "side": "no", "action": "buy", "quantity": 37, "price": 0.145}
    racer_resp = {"orderId": 991234567, "quantityTraded": 37, "fillPrice": 0.855}
    sniper_req = {"exchangeId": "1201", "side": "yes", "action": "buy", "quantity": 10, "price": 0.55}
    sniper_resp = {"orderId": 991234568, "quantityTraded": 10, "fillPrice": 0.55}
    journal = (
        _order_roundtrip(T_NS, RACER_IDEM, "c1", racer_req, racer_resp, 180_000)
        + _order_roundtrip(T_NS + 2, SNIPER_IDEM, "c2", sniper_req, sniper_resp, 90_000)
        + _line(ts_ns=T_NS + 4, direction="event", path="risk/breaker", body_json='{"why":"test"}')
        + "{not json}\n"
    )
    jdir = home.journal / DAY.isoformat()
    jdir.mkdir(parents=True)
    (jdir / "bridge.journal.jsonl").write_text(journal)
    intents = [
        _intent("racer:1", RACER_IDEM, "racer", ops_pb2.INTENT_STATE_DONE, exchange_id="1381"),
        _intent("sniper:1", SNIPER_IDEM, "sniper", ops_pb2.INTENT_STATE_ACKED, exchange_id="1201"),
        _intent(
            "sniper:2", "k3", "sniper", ops_pb2.INTENT_STATE_REJECTED, last_error_code="VALIDATION_ERROR"
        ),
        ops_pb2.OrderIntent(intent_id="old", strategy="sniper", created_at_ns=T_NS - 3 * 86_400 * 10**9),
    ]
    idir = home.state / "bridge"
    idir.mkdir(parents=True)
    (idir / "intents.jsonl").write_text(
        "".join(json_format.MessageToJson(i, indent=None) + "\n" for i in intents)
    )


def _seed_status_and_targets(home) -> None:
    status = ops_pb2.StatusSnapshot(
        mode="paper",
        equity=101_234.0,
        pnl_day=321.0,
        pnl_total=1234.0,
        rank=42,
        field_size=1800,
        events=["sniper paused 3 min"],
    )
    atomic_write_text(home.status, json_format.MessageToJson(status))
    fair = [
        targets_pb2.FairValue(exchange_id="1381", market_id="381", p_yes=0.86, p_lo=0.8, p_hi=0.9),
        targets_pb2.FairValue(exchange_id="1201", market_id="201", p_yes=0.60, p_lo=0.5, p_hi=0.7),
    ]
    tf = build_target_file(
        tournament_slug="midterm-elections",
        seq=1,
        now_ns=T_NS - 3600 * 10**9,
        valid_for_s=36 * 3600,
        inputs=[InputRef("x", "0" * 64, T_NS)],
        fair=fair,
        positions=[],
        cluster_limits={"national": 0.25},
        tourney=targets_pb2.TourneyEstimate(p_first=0.03, p_top3=0.075, field_size=1800, scenario="N2"),
        notes="",
    )
    write_targets(home.targets, tf, pointer_seq=1)


def _seed(home) -> None:
    _seed_journal_and_intents(home)
    _seed_status_and_targets(home)
    prices = pl.DataFrame(
        [
            {
                "ts": T + timedelta(minutes=m),
                "exchange_id": str(e),
                "market_id": str(e),
                "latest_price": 0.5,
                "best_bid": 0.49,
                "best_ask": 0.49 + 0.01 * (e % 3 + 1),
                "spread": None,
            }
            for m in range(3)
            for e in range(10)
        ]
    ).with_columns(pl.col("ts").cast(pl.Datetime("us", "UTC")))
    (home.data / "cup" / "prices").mkdir(parents=True)
    prices.write_parquet(home.data / "cup" / "prices" / "p.parquet")
    row = {"ts": T, "rank": 1, "profile_id": "pid-1", "username": "zesty-otter-17", "pnl": 5000.0}
    row |= {"trades_count": 1, "volume": 1.0, "win_rate": None, "roi": 0.0, "total": 1800, "my_rank": 42}
    lb = pl.DataFrame([row]).with_columns(pl.col("ts").cast(pl.Datetime("us", "UTC")))
    (home.data / "cup" / "leaderboard").mkdir(parents=True)
    lb.write_parquet(home.data / "cup" / "leaderboard" / "l.parquet")


def test_metrics_from_all_sources(home) -> None:
    _seed(home)
    m = collect(home, DAY, now_ns=T_NS)
    assert (m.mode, m.equity, m.my_rank, m.field_size) == ("paper", 101_234.0, 42, 1800)
    assert m.fills == 2
    assert m.racer_attempts == 1
    assert m.racer_hits == 1
    assert m.racer_latency_ms_p50 == pytest.approx(180.0)
    racer, sniper = m.strategies["racer"], m.strategies["sniper"]
    # Racer bought NO at YES-price 0.855 => short YES 37 @ .855 vs fair .86 => edge 37 * (.855 - .86) < 0.
    assert racer.edge_vs_fair == pytest.approx(-37 * (0.86 - 0.855))
    assert sniper.intents == 2
    assert sniper.rejected == 1
    assert sniper.edge_vs_fair == pytest.approx(10 * 0.05)
    assert m.reject_codes == {"VALIDATION_ERROR": 1}
    assert m.spread_median == pytest.approx(0.02)
    assert m.markets_quoted == 10
    assert m.p_first == pytest.approx(0.03)
    assert m.targets_age_h == pytest.approx(1.0)
    assert any("risk/breaker" in e for e in m.risk_events)
    assert "sniper paused 3 min" in m.risk_events


def test_render_has_every_section(home) -> None:
    _seed(home)
    text = render(collect(home, DAY, now_ns=T_NS))
    headings = ["Top 3 proposed changes", "P&L by strategy", "Execution", "Sponsor spreads", "Tournament"]
    for heading in [*headings, "Risk events"]:
        assert f"## {heading}" in text
    assert "| racer |" in text
    assert "1/1 attempts filled" in text


def test_proposals_are_deterministic_and_capped() -> None:
    m = DailyMetrics(
        DAY,
        senate_gap_pts=6.2,
        fair_flags=2,
        unverified_high_confidence=5,
        national_drift_pts=3.0,
        spread_median=0.015,
        markets_quoted=40,
        reject_codes={"X": 12},
    )
    first, second = propose(m), propose(m)
    assert first == second
    assert [p.rule_id for p in first] == ["calibration-gap", "mapping-flags", "rejects"]
    assert propose(DailyMetrics(DAY)) == []
    crowd = propose(DailyMetrics(DAY, national_drift_pts=-2.5))
    assert crowd[0].rule_id == "crowd-drift"
    assert "field is long R" in crowd[0].text


def test_packet_contains_only_aggregates(home) -> None:
    _seed(home)
    daily, packet = run_report(home, home.load_config(), DAY, now_ns=T_NS)
    assert daily.name == "2026-10-05.md"
    assert daily.exists()
    assert sorted(p.name for p in packet.iterdir()) == ["metrics.csv", "strategies.csv", "summary.md"]
    blob = "".join(p.read_text() for p in packet.iterdir())
    for secret in (RACER_IDEM, "991234567", "zesty-otter-17", "pid-1", "1381", "risk/breaker"):
        assert secret not in blob
    metrics = dict(line.split(",", 1) for line in (packet / "metrics.csv").read_text().splitlines()[1:])
    assert metrics["fills"] == "2"
    assert metrics["mode"] == "paper"


@pytest.mark.parametrize(
    ("text", "found"),
    [
        ("all good: fills=3, edge=+1.2", False),
        (f"key {RACER_IDEM}", True),
        ("orderId: 5", True),
        ("order 991234567 filled", True),
        ("top player zesty-otter-17 jumped", True),
        ("exchange_id,1381", True),
    ],
)
def test_scrubber(text: str, found: bool) -> None:
    assert bool(scrub_text(text, usernames=["zesty-otter-17"])) is found


def test_leaking_packet_is_refused_and_removed(home) -> None:
    bad = DailyMetrics(DAY, strategies={"zesty-otter-17": StrategyStats("zesty-otter-17")})
    with pytest.raises(PacketLeakError, match="username"):
        write_packet(home.reports, bad, usernames=["zesty-otter-17"])
    assert not (home.reports / "packets" / DAY.isoformat()).exists()
    assert not list((home.reports / "packets").glob(".*"))
    assert write_packet(home.reports, DailyMetrics(DAY, mode="paper")).exists()


def test_journal_loader_reads_bridge_paths_skips_price_polls_and_zst(home) -> None:
    """The bridge journals `/api/v1/...` paths (the Oct 1-6 reports counted 0 fills because of the
    prefix), price polls dominate the bytes, and days older than yesterday are zstd-compressed."""
    import zstandard
    from susq.reports.sources import extract_fills, load_journal

    req = {"exchangeId": "1201", "side": "yes", "action": "buy", "quantity": 10, "price": 0.55}
    resp = {"orderId": 1, "quantityTraded": 10, "fillPrice": 0.55}
    common = {
        "method": "POST",
        "path": "/api/v1/orders",
        "idempotency_key": SNIPER_IDEM,
        "correlation_id": "c9",
    }
    poll = _line(
        ts_ns=T_NS + 5, direction="response", method="GET", status=200,
        path="/api/v1/exchanges/prices?ids=1,2", body_json='{"prices":[{"exchangeId":"1"}]}',
    )  # fmt: skip
    text = (
        _line(ts_ns=T_NS, direction="request", body_json=json.dumps(req), **common)
        + _line(ts_ns=T_NS + 1, direction="response", status=200, body_json=json.dumps(resp), **common)
        + poll
        + _line(ts_ns=T_NS + 6, direction="event", path="risk/breaker", body_json='{"why":"x"}')
    )
    jdir = home.journal / DAY.isoformat()
    jdir.mkdir(parents=True)
    (jdir / "bridge.journal.jsonl.zst").write_bytes(zstandard.compress(text.encode()))
    entries = load_journal(home.journal, DAY)
    assert [e.path for e in entries] == ["/orders", "/orders", "risk/breaker"]
    fills = extract_fills(entries, {})
    assert len(fills) == 1
    assert (fills[0].exchange_id, fills[0].quantity) == ("1201", 10)
