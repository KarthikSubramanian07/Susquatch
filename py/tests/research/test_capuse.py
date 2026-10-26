"""EXP-015: true cash, cap use and scenario refusals in the daily packet."""

from __future__ import annotations

import json
from datetime import UTC, date, datetime

import pytest
from google.protobuf import json_format
from susq.calibrate.targets import InputRef, build_target_file, write_targets
from susq.contracts import ops_pb2, targets_pb2
from susq.mapping.io import write_market_map
from susq.reports.capuse import cap_use, cluster_cap_frac, scenario_refusals
from susq.reports.job import run_report
from susq.reports.metrics import DailyMetrics, collect
from susq.reports.rules import propose
from susq.research_env import atomic_write_text

DAY = date(2026, 10, 25)
T_NS = int(datetime(2026, 10, 25, 15, tzinfo=UTC).timestamp() * 1e9)
SLUG = "midterm-elections"
CAPS = {"national": 0.90, "region": 0.40, "state": 0.20, "house": 0.20}
CLUSTERS = {
    "1001": ["national", "region:south", "house"],
    "1002": ["national", "region:south", "state:GA"],
}


def _entry(t: int, *, producer: str = "bridge", **kw) -> ops_pb2.JournalEntry:
    return ops_pb2.JournalEntry(ts_ns=t, producer=producer, **kw)


def _get(t: int, path: str, body: dict, *, producer: str = "bridge") -> ops_pb2.JournalEntry:
    return _entry(
        t,
        producer=producer,
        direction="response",
        method="GET",
        status=200,
        path=path,
        body_json=json.dumps(body),
    )


def _event(t: int, kind: str, body: dict, *, producer: str = "bridge") -> ops_pb2.JournalEntry:
    return _entry(t, producer=producer, direction="event", method=kind, body_json=json.dumps(body))


def _pnl(account: float, holdings: float) -> dict:
    return {"period": "all", "totalAccountValue": account, "totalHoldingsValue": holdings}


def _order(ex: str, side: str, action: str, qty: int, limit: float) -> dict:
    return {"id": 7, "exchangeId": ex, "side": side, "action": action, "quantity": qty, "priceLimit": limit}


def _page(orders: list[dict], cursor: str | None) -> dict:
    return {"data": orders, "pagination": {"hasMore": cursor is not None, "nextCursor": cursor}}


POSITIONS = {
    "positions": [
        {"exchangeId": "1001", "quantity": 1000, "currentPrice": 0.6},  # YES: 600 at mark
        {"exchangeId": "1002", "quantity": -2000, "currentPrice": 0.3},  # NO: 2000 x 0.7 = 1400
        {"exchangeId": "1003", "quantity": 500},  # no mark: a full unit per share = 500
    ]
}
ORDERS = "/orders?tournamentId=t&status=open&limit=200"


def _reads(t: int = T_NS) -> list[ops_pb2.JournalEntry]:
    """The bridge's reconcile reads, with superseded and filtered reads around them."""
    return [
        _get(t, f"/tournaments/{SLUG}/portfolio/pnl?period=all", _pnl(90_000.0, 60_000.0)),  # superseded
        _get(t + 1, ORDERS, _page([_order("1001", "yes", "buy", 9_999, 0.5)], None)),  # superseded scan
        _get(t + 10, f"/tournaments/{SLUG}/portfolio/pnl?period=all", _pnl(100_000.0, 82_000.0)),
        _get(t + 11, f"/tournaments/{SLUG}/portfolio/positions", POSITIONS),
        _get(t + 12, ORDERS, _page([_order("1001", "yes", "buy", 100, 0.5)], "c1")),  # 50
        _get(t + 13, "/orders?tournamentId=t&status=open&exchangeId=1002", _page([], None)),  # filtered
        _get(t + 14, ORDERS + "&cursor=c1", _page([_order("1002", "no", "buy", 200, 0.4)], None)),  # 80
    ]


def test_cash_and_cap_use_from_the_last_reads() -> None:
    use = cap_use(_reads(), clusters_by_exchange=CLUSTERS, kind_caps=CAPS)
    assert use.equity == pytest.approx(100_000.0)
    assert use.cash == pytest.approx(18_000.0)
    assert use.cash_frac == pytest.approx(0.18)
    assert use.total_gross == pytest.approx(650 + 1480 + 500)
    assert use.total_gross_frac == pytest.approx(0.0263)
    # state:GA 1480 / 20k, region:south 2130 / 40k, house 650 / 20k, national 2130 / 90k.
    assert use.top_clusters() == [
        ("state:GA", pytest.approx(0.074)),
        ("region:south", pytest.approx(0.05325)),
        ("house", pytest.approx(0.0325)),
    ]


def test_cap_use_without_portfolio_reads_reports_nothing() -> None:
    use = cap_use([], clusters_by_exchange=CLUSTERS, kind_caps=CAPS)
    assert (use.cash, use.cash_frac, use.total_gross_frac, use.top_clusters()) == (None, None, None, [])
    # pnl only: cash is known, cap use is not (an incomplete scan never counts).
    pnl_only = [*_reads()[:3], _get(T_NS + 20, ORDERS, _page([], "c9"))]
    use = cap_use(pnl_only, clusters_by_exchange=CLUSTERS, kind_caps=CAPS)
    assert use.cash == pytest.approx(18_000.0)
    assert use.total_gross_frac is None


def test_cluster_caps_take_the_tightest_of_kind_and_override() -> None:
    assert cluster_cap_frac("region:south", CAPS, {}) == 0.40
    assert cluster_cap_frac("region:south", CAPS, {"region:south": 0.30}) == 0.30
    assert cluster_cap_frac("national", CAPS, {"national": 0.95}) == 0.90
    assert cluster_cap_frac("county:x", CAPS, {}) is None


def _scenario(t: int, status: str, reason: str | None) -> ops_pb2.JournalEntry:
    return _event(t, "scenario_order", {"exchangeId": "1001", "status": status, "reason": reason})


def _blocked(t: int, strategy: str, reason: str) -> ops_pb2.JournalEntry:
    order = {"legs": [{"strategy": strategy, "exchange_id": "1001"}], "atomic": False}
    return _event(t, "blocked", {"reason": reason, "order": order}, producer="rust-live")


def test_scenario_refusals_on_caps() -> None:
    events = [
        _scenario(T_NS, "risk_rejected", "cap: total_gross, cluster:region:south"),
        _scenario(T_NS + 1, "risk_rejected", "cap: cluster:house"),
        _scenario(T_NS + 2, "risk_rejected", "locked_leg"),  # refused, but not on a cap
        _scenario(T_NS + 3, "placed", None),
        _blocked(T_NS + 4, "scenario", "risk: cluster cap on region:south: 41000.00 > 40000.00"),
        _blocked(T_NS + 5, "scenario", "killed: ops"),
        _blocked(T_NS + 6, "sniper", "risk: gross cap: 91000.00 > 90000.00"),
    ]
    # The bridge is the live trader: rust-live's events are the paper run's.
    assert scenario_refusals(events, rust_live_trader=False, rust_sent=5) == (4, 2)
    # rust-live is the live trader: its scenario refusals and sent intents count too.
    assert scenario_refusals(events, rust_live_trader=True, rust_sent=3) == (4 + 2 + 3, 3)


def _seed(home, *, trader: str = "bridge") -> None:
    status = ops_pb2.StatusSnapshot(mode="live", trader=trader, equity=100_000.0, cash=27_254.41)
    atomic_write_text(home.status, json_format.MessageToJson(status))
    rules = [ops_pb2.MarketRule(exchange_id=ex, clusters=c) for ex, c in CLUSTERS.items()]
    write_market_map(home.market_map, ops_pb2.MarketMap(markets=rules))
    tf = build_target_file(
        tournament_slug=SLUG,
        seq=1,
        now_ns=T_NS - 3600 * 10**9,
        valid_for_s=36 * 3600,
        inputs=[InputRef("x", "0" * 64, T_NS)],
        fair=[],
        positions=[],
        cluster_limits={"national": 0.025},  # tighter than [risk.clusters] national = 0.90
        tourney=targets_pb2.TourneyEstimate(),
        notes="",
    )
    write_targets(home.targets, tf, pointer_seq=1)
    entries = [
        *_reads(),
        # Another producer's later reads (rust-live's paper run) are not the live trader's.
        _get(
            T_NS + 30, f"/tournaments/{SLUG}/portfolio/pnl?period=all", _pnl(1.0, 0.5), producer="rust-live"
        ),
        _scenario(T_NS + 40, "risk_rejected", "cap: total_gross"),
        _scenario(T_NS + 41, "risk_rejected", "cap: cluster:house"),
        _scenario(T_NS + 42, "placed", None),
    ]
    jdir = home.journal / DAY.isoformat()
    jdir.mkdir(parents=True)
    text = "".join(json_format.MessageToJson(e, indent=None) + "\n" for e in entries)
    (jdir / "bridge.journal.jsonl").write_text(text)


def test_collect_reports_true_cash_cap_use_and_refusals(home) -> None:
    _seed(home)
    m = collect(home, DAY, now_ns=T_NS, cluster_limits=CAPS)
    assert m.cash == pytest.approx(18_000.0)  # not status.json's start-up 27,254.41
    assert m.cash_frac == pytest.approx(0.18)
    assert m.total_gross_frac == pytest.approx(0.0263)
    # The targets file's national override (2.5%) makes national the closest to its cap.
    assert [c for c, _ in m.cluster_use] == ["national", "state:GA", "region:south"]
    assert m.cluster_use[0][1] == pytest.approx(2130 / 2500)
    assert (m.scenario_orders, m.scenario_refused_caps) == (3, 2)


def test_packet_has_cash_and_cap_rows_and_flags_a_cap_blocked_day(home) -> None:
    _seed(home)
    _, packet = run_report(home, home.load_config(), DAY, now_ns=T_NS)
    lines = (packet / "metrics.csv").read_text().splitlines()[1:]
    metrics = dict(line.split(",", 1) for line in lines)
    assert metrics["cash"] == "18000"
    assert metrics["cash_frac"] == "0.18"
    assert metrics["total_gross_frac"] == "0.0263"
    assert metrics["cluster_1"] == "national"
    assert float(metrics["cluster_1_cap_use"]) == pytest.approx(0.852)
    assert metrics["cluster_3"] == "region:south"
    assert (metrics["scenario_orders"], metrics["scenario_refused_caps"]) == ("3", "2")
    assert "[scenario-caps] 2 of 3 scenario orders (67%)" in (packet / "summary.md").read_text()


def test_scenario_caps_rule_needs_a_majority() -> None:
    assert propose(DailyMetrics(DAY, scenario_orders=4, scenario_refused_caps=2)) == []
    hit = propose(DailyMetrics(DAY, scenario_orders=4, scenario_refused_caps=3, total_gross_frac=0.854))
    assert [p.rule_id for p in hit] == ["scenario-caps"]
    assert "0.854 of equity" in hit[0].text
