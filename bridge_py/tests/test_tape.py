"""Trade-tape recorder: schema, dedupe, day partitions, cursor resume, the request cap, the loop switch."""

from __future__ import annotations

import asyncio
import json
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pyarrow.parquet as pq
import pytest
from susq.platform.models import Pagination, Trade, TradesPage
from susq_bridge.config import load_config
from susq_bridge.recorder import Recorder, TapeCursor
from susq_bridge.runtime import Loops

T0 = datetime(2026, 10, 5, 12, 0, tzinfo=UTC)


def _trade(n: int, *, at: datetime | None = None, side: str = "YES") -> Trade:
    """Trade ``n`` happened ``n`` minutes after Oct 1 unless ``at`` says otherwise."""
    return Trade(
        id=f"t{n}",
        created_at=at or (datetime(2026, 10, 1, tzinfo=UTC) + timedelta(minutes=n)),
        price=0.5,
        size=n,
        side=side,
        volume=0.5 * n,
    )


@dataclass
class FakeClient:
    """``get_trades`` over an in-memory tape per exchange: newest first, opaque offset cursors."""

    tapes: dict[str, list[Trade]]
    calls: list[tuple[str, str | None, str | None]] = field(default_factory=list)

    async def get_trades(
        self,
        exchange_id: str,
        tournament_id: str,
        *,
        limit: int = 50,
        cursor: str | None = None,
        from_: str | None = None,
        to: str | None = None,
    ) -> TradesPage:
        self.calls.append((exchange_id, cursor, from_))
        newest_first = sorted(
            self.tapes.get(exchange_id, []), key=lambda t: (t.created_at, t.id), reverse=True
        )
        start = int(cursor) if cursor else 0
        chunk = newest_first[start : start + limit]
        more = start + limit < len(newest_first)
        return TradesPage(
            exchange_id=exchange_id,
            market_id=f"m-{exchange_id}",
            data=chunk,
            pagination=Pagination(
                limit=limit, has_more=more, next_cursor=str(start + limit) if more else None
            ),
        )


@dataclass
class FakeGateway:
    client: FakeClient
    labels: list[str] = field(default_factory=list)

    async def read[T](self, label: str, fn: Callable[[], Awaitable[T]], **_: object) -> T:
        self.labels.append(label)
        return await fn()


@dataclass
class FakeMarket:
    exchange_market: dict[str, str]
    tournament_id: str = "tid"


def _recorder(tmp_path: Path) -> Recorder:
    return Recorder(tmp_path, clock=lambda: T0)


def _rows(paths: list[Path]) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for p in paths:
        rows.extend(pq.read_table(p).to_pylist())
    return rows


def test_write_tape_schema_dedupe_and_day_partitions(tmp_path: Path) -> None:
    rec = _recorder(tmp_path)
    oct1 = _trade(1)
    oct3 = _trade(3, at=datetime(2026, 10, 3, 9, 30, tzinfo=UTC), side="no")
    paths = rec.write_tape("1301", [oct1, oct1, oct3], market_id="301")
    assert [str(p.relative_to(tmp_path)) for p in paths] == [
        "cup/tape/date=2026-10-01/part-20261005T120000000000Z.parquet",
        "cup/tape/date=2026-10-03/part-20261005T120000000000Z.parquet",
    ]
    schema = pq.read_schema(paths[0])
    assert {f.name: str(f.type) for f in schema} == {
        "ts": "timestamp[us, tz=UTC]",
        "exchange_id": "large_string",
        "market_id": "large_string",
        "trade_id": "large_string",
        "side": "large_string",
        "price": "double",
        "size": "int64",
        "volume": "double",
        "recorded_at": "timestamp[us, tz=UTC]",
    }
    assert pq.read_metadata(paths[0]).row_group(0).column(0).compression == "ZSTD"
    rows = _rows(paths)
    assert [r["trade_id"] for r in rows] == ["t1", "t3"]  # the duplicate t1 collapsed
    assert rows[0]["market_id"] == "301"
    assert rows[1]["side"] == "NO"  # normalised
    assert rows[1]["ts"] == datetime(2026, 10, 3, 9, 30, tzinfo=UTC)
    assert rows[0]["recorded_at"] == T0
    assert rec.write_tape("1301", []) == []
    assert not any(p.name.startswith(".") for p in paths[0].parent.iterdir())  # no tmp files left


async def test_record_tape_resumes_from_the_cursor_across_passes(tmp_path: Path) -> None:
    rec = _recorder(tmp_path)
    client = FakeClient({"A": [_trade(n) for n in range(1, 6)], "B": [_trade(10)]})
    gw = FakeGateway(client)
    market = FakeMarket({"A": "mA", "B": "mB"})

    first = await rec.record_tape(gw, market, max_requests=600)  # type: ignore[arg-type]
    assert sorted(r["trade_id"] for r in _rows(first)) == ["t1", "t10", "t2", "t3", "t4", "t5"]
    assert {r["market_id"] for r in _rows(first)} == {"mA", "mB"}
    assert set(gw.labels) == {"tape"}
    assert len(client.calls) == 2  # one page per exchange
    state = json.loads(rec.tape_state_path.read_text())["exchanges"]
    assert state["A"]["last_id"] == "t5"
    assert state["A"]["pending_cursor"] is None
    assert state["B"]["last_id"] == "t10"

    # Nothing new: one read per exchange, the floor is passed as ``from``, nothing written.
    client.calls.clear()
    assert await rec.record_tape(gw, market, max_requests=600) == []  # type: ignore[arg-type]
    assert sorted(client.calls) == [
        ("A", None, _trade(5).created_at.isoformat()),
        ("B", None, _trade(10).created_at.isoformat()),
    ]  # type: ignore[union-attr]

    # Two new trades on A land in a fresh file; the old ones are not written again.
    client.tapes["A"] += [_trade(6), _trade(7)]
    client.calls.clear()
    second = await rec.record_tape(gw, market, max_requests=600)  # type: ignore[arg-type]
    assert sorted(r["trade_id"] for r in _rows(second)) == ["t6", "t7"]
    assert len(client.calls) == 2
    assert rec.load_tape_state()["A"].last_id == "t7"


async def test_request_cap_bounds_a_pass_and_the_next_pass_finishes_the_walk(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("susq_bridge.recorder.TAPE_PAGE_SIZE", 2)
    rec = _recorder(tmp_path)
    client = FakeClient({"A": [_trade(n) for n in range(1, 8)], "B": [_trade(20)]})
    gw = FakeGateway(client)
    market = FakeMarket({"A": "mA", "B": "mB"})

    first = await rec.record_tape(gw, market, max_requests=2)  # type: ignore[arg-type]
    assert len(client.calls) == 2  # the cap, not the backlog, decides the pass size
    assert sorted(r["trade_id"] for r in _rows(first)) == ["t4", "t5", "t6", "t7"]
    cur = rec.load_tape_state()["A"]
    assert cur.pending_cursor == "4"
    assert cur.head_id == "t7"
    assert cur.last_id is None  # the floor only moves once the walk reaches the old floor
    assert "B" not in rec.load_tape_state()  # never reached: no read spent, nothing to resume

    # The unfinished walk resumes from its cursor (first, before the untouched exchange).
    client.calls.clear()
    second = await rec.record_tape(gw, market, max_requests=3)  # type: ignore[arg-type]
    assert client.calls[0] == ("A", "4", None)
    assert sorted(r["trade_id"] for r in _rows(second)) == ["t1", "t2", "t20", "t3"]
    state = rec.load_tape_state()
    assert (state["A"].last_id, state["A"].pending_cursor, state["A"].head_id) == ("t7", None, None)
    assert state["B"].last_id == "t20"

    # Steady state: a trade since the head is picked up at the top with the floor as ``from``.
    client.tapes["A"].append(_trade(8))
    client.calls.clear()
    third = await rec.record_tape(gw, market, max_requests=3)  # type: ignore[arg-type]
    assert [r["trade_id"] for r in _rows(third)] == ["t8"]
    assert client.calls[0] == ("A", None, _trade(7).created_at.isoformat())  # type: ignore[union-attr]
    assert len(client.calls) == 2


async def test_a_failing_exchange_does_not_sink_the_pass(tmp_path: Path) -> None:
    rec = _recorder(tmp_path)
    client = FakeClient({"A": [_trade(1)], "B": [_trade(2)]})

    async def boom(exchange_id: str, *a: object, **kw: object) -> TradesPage:
        if exchange_id == "A":
            raise RuntimeError("platform 500")
        return await FakeClient.get_trades(client, exchange_id, *a, **kw)  # type: ignore[arg-type]

    client.get_trades = boom  # type: ignore[method-assign]
    gw = FakeGateway(client)
    paths = await rec.record_tape(gw, FakeMarket({"A": "mA", "B": "mB"}), max_requests=10)  # type: ignore[arg-type]
    assert [r["trade_id"] for r in _rows(paths)] == ["t2"]
    state = rec.load_tape_state()
    assert state["A"] == TapeCursor()  # restarted from the top next pass
    assert state["B"].last_id == "t2"


async def test_only_one_pass_in_flight(tmp_path: Path) -> None:
    rec = _recorder(tmp_path)
    rec._tape_busy = True
    assert await rec.record_tape(FakeGateway(FakeClient({})), FakeMarket({"A": "m"})) == []  # type: ignore[arg-type]


def test_unreadable_state_restarts_from_the_top(tmp_path: Path) -> None:
    rec = _recorder(tmp_path)
    rec.tape_state_path.parent.mkdir(parents=True)
    rec.tape_state_path.write_text("{not json")
    assert rec.load_tape_state() == {}
    rec.save_tape_state({"A": TapeCursor(last_id="t1", last_created_at=T0)})
    assert rec.load_tape_state() == {"A": TapeCursor(last_id="t1", last_created_at=T0)}


async def test_tape_against_the_mock_exchange(harness, live_bridge) -> None:  # type: ignore[no-untyped-def]
    """End to end through the real gateway and limiter: every mock exchange has one print at T0."""
    rec = live_bridge.recorder
    paths = await rec.record_tape(live_bridge.gateway, live_bridge.market, max_requests=600)
    rows = _rows(paths)
    assert len(rows) == 19
    assert {r["exchange_id"] for r in rows} == set(live_bridge.market.exchange_market)
    assert all(r["market_id"] == live_bridge.market.exchange_market[r["exchange_id"]] for r in rows)
    assert {r["side"] for r in rows} <= {"YES", "NO"}
    assert all(0 < r["price"] < 1 for r in rows)
    assert await rec.record_tape(live_bridge.gateway, live_bridge.market, max_requests=600) == []


def test_tape_config_defaults_and_budget(home: Path) -> None:
    cfg = load_config(home, env={})
    assert cfg.tape_interval_s == 1800
    assert cfg.tape_max_requests_per_pass == 600
    # Worst case: the whole cap every pass is a fifth of the 100 reads/min account budget.
    assert cfg.tape_max_requests_per_pass / (cfg.tape_interval_s / 60) <= 20


async def test_tape_loop_is_wired_only_when_enabled(harness, monkeypatch: pytest.MonkeyPatch) -> None:  # type: ignore[no-untyped-def]
    quiet = Loops(realtime=False, racer=False, sniper=False, farquote=False)

    async def run_once(interval: float) -> tuple[set[str], list[int]]:
        bridge = await harness.open_bridge("paper")
        bridge.config = replace(bridge.config, tape_interval_s=interval)
        calls: list[int] = []

        async def fake_tape(gateway: object, market: object, *, max_requests: int) -> list[Path]:
            calls.append(max_requests)
            return []

        monkeypatch.setattr(bridge.recorder, "record_tape", fake_tape)
        run = asyncio.ensure_future(bridge.run(quiet))
        try:
            await asyncio.sleep(0.05)
            return {t.get_name() for t in bridge.tasks}, calls
        finally:
            run.cancel()
            for t in bridge.tasks:
                t.cancel()
            await asyncio.gather(run, *bridge.tasks, return_exceptions=True)
            await bridge.close()

    names, calls = await run_once(1800)
    assert "recorder-tape" in names
    assert calls == [harness.config.tape_max_requests_per_pass]  # the cap from config reaches the pass
    names, calls = await run_once(0)
    assert "recorder-tape" not in names
    assert calls == []
