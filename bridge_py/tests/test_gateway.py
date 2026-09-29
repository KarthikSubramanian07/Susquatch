"""The gateway against the mock: routing, retries, gates, kill, lease, restart recovery."""

from __future__ import annotations

from dataclasses import replace
from datetime import timedelta

import orjson
import pytest
from susq.contracts import ops_pb2
from susq.platform.models import OrderRequest
from susq_bridge.gateway import OrderSpec
from susq_bridge.lease import Lease
from susq_bridge.runtime import Bridge

EX = "1309"  # Michigan D: no stale student orders at seed 7


def resting_spec(bid: int, qty: int = 5, ttl_s: float | None = None) -> OrderSpec:
    return OrderSpec(EX, "yes", "buy", qty, bid - 200, ttl_s=ttl_s)


async def mock_open_orders(harness) -> list:  # type: ignore[no-untyped-def,type-arg]
    return await harness.mock.client.list_open_orders(harness.mock.tid)


async def test_live_routes_to_the_platform_and_stamps_expiry(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    out = await live_bridge.gateway.place("t:live:1", "test", resting_spec(bid))
    assert out.placed
    opens = await mock_open_orders(harness)
    assert [o.id for o in opens] == [out.result.order_id]
    assert opens[0].expiration_date == harness.now() + timedelta(seconds=300)
    intent = live_bridge.intents.get("t:live:1")
    assert intent.state == ops_pb2.INTENT_STATE_ACKED
    assert intent.idempotency_key == live_bridge.intents.key_for("t:live:1", out.request)
    assert [o.order_id for o in live_bridge.oms.open_orders()] == [out.result.order_id]


async def test_paper_never_touches_the_platform(harness, paper_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    _, ask = await harness.top(EX)
    out = await paper_bridge.gateway.place("t:paper:1", "test", OrderSpec(EX, "yes", "buy", 3, ask))
    assert out.placed
    assert out.result.quantity_traded == 3  # filled against the (mock) real book
    assert await mock_open_orders(harness) == []
    assert (await harness.mock.client.get_positions("midterm-elections")).positions == []
    assert paper_bridge.gateway.paper.ledger.position(EX) == 3


async def test_order_status_unknown_is_retried_and_placed_exactly_once(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    await harness.mock.post_admin("/_mock/faults", {"order_status_unknown_next": 1})
    out = await live_bridge.gateway.place("t:502", "test", resting_spec(bid))
    assert out.placed
    assert out.attempts == 2
    opens = await mock_open_orders(harness)
    assert len(opens) == 1
    assert opens[0].id == out.result.order_id
    assert live_bridge.intents.get("t:502").state == ops_pb2.INTENT_STATE_ACKED
    keys = {
        line["idempotencyKey"] for line in _journal(live_bridge) if line.get("path", "").endswith("/orders")
    }
    assert keys == {out.idempotency_key}


async def test_service_unavailable_and_rate_limit_retry_with_backoff(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    await harness.mock.post_admin("/_mock/faults", {"service_unavailable_next": 1})
    out = await live_bridge.gateway.place("t:503", "test", resting_spec(bid))
    assert out.placed
    assert out.attempts == 2
    rps_before = live_bridge.limiter.rps
    await harness.mock.post_admin("/_mock/faults", {"rate_limit_rps": 1, "retry_after_s": 3})
    await harness.mock.client.get_account()  # use up this second's budget
    out = await live_bridge.gateway.place("t:429", "test", resting_spec(bid - 50))
    assert out.placed
    assert 3.0 in harness.sleeps  # honoured Retry-After
    assert live_bridge.limiter.rps < rps_before


async def test_request_in_flight_waits_at_least_90s(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    await harness.mock.post_admin("/_mock/faults", {"request_in_flight_next": 1})
    out = await live_bridge.gateway.place("t:409", "test", resting_spec(bid, ttl_s=600))
    assert out.placed
    assert max(harness.sleeps) >= 90


async def test_retries_stop_at_the_orders_own_expiry(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    await harness.mock.post_admin("/_mock/faults", {"request_in_flight_next": 1})
    out = await live_bridge.gateway.place("t:short", "test", resting_spec(bid, ttl_s=20))
    assert out.status == "unknown"
    assert live_bridge.intents.get("t:short").state == ops_pb2.INTENT_STATE_UNKNOWN
    assert await mock_open_orders(harness) == []


async def test_terminal_errors_are_not_retried(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    # Loosen risk so the platform, not risk, rejects the order (insufficient balance -> 400).
    live_bridge.risk.config = replace(
        live_bridge.risk.config,
        max_order_notional_frac=100.0,
        per_market_gross_frac=100.0,
        total_gross_frac=100.0,
    )
    live_bridge.risk.equity = 10_000_000.0
    live_bridge.risk.cash = None  # unknown cash: risk's buying-power check stands aside
    out = await live_bridge.gateway.place("t:400", "test", OrderSpec(EX, "yes", "buy", 1_000_000, 5_000))
    assert out.status == "platform_rejected"
    assert out.reason == "VALIDATION_ERROR"
    assert out.attempts == 1
    assert live_bridge.intents.get("t:400").state == ops_pb2.INTENT_STATE_REJECTED


async def test_duplicate_intent_is_not_resent(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    first = await live_bridge.gateway.place("t:dup", "test", resting_spec(bid))
    second = await live_bridge.gateway.place("t:dup", "test", resting_spec(bid))
    assert first.placed
    assert second.status == "duplicate"
    assert len(await mock_open_orders(harness)) == 1


async def test_time_gate_blocks_new_orders_and_caps_expiry(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    last = live_bridge.config.calendar.last_order_utc
    harness.mx.clock.set(last - timedelta(minutes=1))
    assert live_bridge.lease.acquire()  # the clock jumped a month; the old lease has long expired
    bid, _ = await harness.top(EX)
    out = await live_bridge.gateway.place("t:gate:1", "test", resting_spec(bid, ttl_s=3600))
    assert out.placed
    assert out.request.expiration == last
    harness.mx.clock.set(last)
    assert live_bridge.lease.acquire()
    out = await live_bridge.gateway.place("t:gate:2", "test", resting_spec(bid - 50))
    assert (out.status, out.reason) == ("blocked", "time_gate")
    harness.mx.clock.set(last - timedelta(seconds=1))
    assert live_bridge.lease.acquire()
    out = await live_bridge.gateway.place("t:gate:3", "test", resting_spec(bid - 100))
    assert out.status == "blocked"  # an expiry under 2 s away is refused too


async def test_writes_need_the_lease(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    live_bridge.lease.release()
    rival = Lease(
        live_bridge.config.lease_path,
        "rust-live",
        ttl_s=30,
        pid=999_999,
        clock_ns=lambda: int(harness.now().timestamp() * 1e9),
    )
    assert rival.acquire()
    bid, _ = await harness.top(EX)
    out = await live_bridge.gateway.place("t:lease", "test", resting_spec(bid))
    assert (out.status, out.reason) == ("blocked", "no_lease")
    assert not await live_bridge.gateway.cancel(1)
    assert await mock_open_orders(harness) == []


async def test_kill_mid_run_cancels_everything_and_freezes(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    for i in range(3):
        assert (await live_bridge.gateway.place(f"t:k:{i}", "test", resting_spec(bid - 50 * i))).placed
    assert len(await mock_open_orders(harness)) == 3
    live_bridge.kill.trigger("drill: kill mid-run")
    out = await live_bridge.gateway.place("t:k:after", "test", resting_spec(bid - 300))
    assert out.status == "blocked"
    assert "drill" in (out.reason or "")
    assert await mock_open_orders(harness) == []
    assert live_bridge.risk.frozen
    assert live_bridge.oms.open_orders() == []


async def test_breaker_trip_cancels_all(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    assert (await live_bridge.gateway.place("t:b:1", "test", resting_spec(bid))).placed
    for _ in range(live_bridge.config.risk.reject_breaker_per_min + 1):
        live_bridge.risk.record_reject()
    assert live_bridge.risk.frozen
    for task in list(live_bridge.gateway._background):
        await task
    assert await mock_open_orders(harness) == []


async def test_cancel_all_confirms_after_a_paused_sweep(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    for i in range(4):
        await live_bridge.gateway.place(f"t:c:{i}", "test", resting_spec(bid - 50 * i))
    await harness.mock.post_admin("/_mock/faults", {"cancel_all_paused_next": 5})
    assert await live_bridge.gateway.cancel_all("test")
    assert await mock_open_orders(harness) == []


async def test_cancel_treats_already_closed_as_done(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    out = await live_bridge.gateway.place("t:x", "test", resting_spec(bid))
    oid = out.result.order_id
    assert await live_bridge.gateway.cancel(oid)
    assert await live_bridge.gateway.cancel(oid)  # 409 already closed -> still "not resting"


async def test_bundle_is_atomic_and_shares_one_key(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    b1, _ = await harness.top("1301")
    b2, _ = await harness.top("1302")
    specs = [OrderSpec("1301", "yes", "buy", 4, b1 - 100), OrderSpec("1302", "yes", "buy", 4, b2 - 100)]
    out = await live_bridge.gateway.place_bundle("t:bundle", "test", specs)
    assert out.placed
    assert len(await mock_open_orders(harness)) == 2
    legs = live_bridge.intents.by_key(out.idempotency_key)
    assert [i.intent_id for i in legs] == ["t:bundle#0", "t:bundle#1"]


async def test_restart_resumes_fresh_intents_without_double_placing(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("live")
    bid, _ = await harness.top(EX)
    expiry = harness.now() + timedelta(seconds=120)
    request = OrderRequest(EX, "yes", "buy", 5, bid - 200, expiry, bridge.tournament.id)
    intent = bridge.intents.create("t:crash", "test", request)
    bridge.intents.update("t:crash", ops_pb2.INTENT_STATE_SENT, bump_attempts=True)
    # The request reached the exchange, then the process died before recording the response.
    placed = await harness.mock.client.place_order(request, intent.idempotency_key)
    bridge.lease.release()
    await bridge.close()

    harness.mx.clock.advance(5)
    restarted = await harness.open_bridge("live")
    try:
        opens = await mock_open_orders(harness)
        assert [o.id for o in opens] == [placed.order_id]  # replayed with the same key, not re-placed
        resumed = restarted.intents.get("t:crash")
        assert resumed.state == ops_pb2.INTENT_STATE_ACKED
        assert resumed.order_id == placed.order_id
        assert resumed.idempotency_key == intent.idempotency_key
    finally:
        await restarted.close()


async def test_restart_closes_out_stale_intents(harness) -> None:  # type: ignore[no-untyped-def]
    bridge = await harness.open_bridge("live")
    bid, _ = await harness.top(EX)
    request = OrderRequest(
        EX, "yes", "buy", 5, bid - 200, harness.now() + timedelta(seconds=300), bridge.tournament.id
    )
    bridge.intents.create("t:stale", "test", request)
    bridge.lease.release()
    await bridge.close()
    harness.mx.clock.advance(120)  # much older than the 30 s resume window
    restarted = await harness.open_bridge("live")
    try:
        assert restarted.intents.get("t:stale").state == ops_pb2.INTENT_STATE_DONE
        assert restarted.intents.get("t:stale").last_error_code == "stale_on_restart"
        assert await mock_open_orders(harness) == []
    finally:
        await restarted.close()


async def test_journal_has_every_request_and_never_the_key(harness, live_bridge: Bridge) -> None:  # type: ignore[no-untyped-def]
    bid, _ = await harness.top(EX)
    await live_bridge.gateway.place("t:j", "test", resting_spec(bid))
    live_bridge.journal.flush()
    raw = b"".join(p.read_bytes() for p in harness.config.journal_dir.rglob("*.jsonl"))
    assert b"mock-key-alpha" not in raw
    lines = _journal(live_bridge)
    assert any(line.get("method") == "POST" and line.get("path") == "/api/v1/orders" for line in lines)
    assert any(line.get("method") == "order_ack" for line in lines)


def _journal(bridge: Bridge) -> list[dict]:  # type: ignore[type-arg]
    bridge.journal.flush()
    out = []
    for path in sorted(bridge.config.journal_dir.rglob("bridge.journal.jsonl")):
        out.extend(orjson.loads(line) for line in path.read_bytes().splitlines() if line.strip())
    return out


@pytest.mark.parametrize("mode", ["paper"])
async def test_paper_is_the_default_mode(harness, mode: str) -> None:  # type: ignore[no-untyped-def]
    from susq_bridge.killswitch import KillSwitch, resolve_mode

    kill = KillSwitch(harness.config.kill_file, harness.config.go_live_file)
    assert resolve_mode(harness.config.mode_default, harness.config.env, kill) == mode
