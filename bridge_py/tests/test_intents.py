"""IntentStore: write-ahead persistence, restart, torn lines, keys and request reconstruction."""

from __future__ import annotations

from collections.abc import Callable
from datetime import UTC, datetime
from pathlib import Path

import pytest
from susq.contracts import common_pb2, ops_pb2
from susq.platform.models import OrderRequest
from susq_bridge.intents import IntentStore

NAMESPACE = "6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99"
VECTOR_INTENT = "racer:rel-9:1791475200"
VECTOR_UUID = "c5fe4b7e-7efc-55fb-82b5-1bd6356d699c"


def order(**overrides: object) -> OrderRequest:
    fields: dict[str, object] = {
        "exchange_id": "381",
        "side": "no",
        "action": "buy",
        "quantity": 37,
        "price_pips": 1450,
        "expiration": datetime.fromtimestamp(1791475230, UTC),
        "tournament_id": "t-uuid",
    }
    fields.update(overrides)
    return OrderRequest(**fields)  # type: ignore[arg-type]


def ticking_clock(start: int = 1_000) -> Callable[[], int]:
    state = {"now": start}

    def clock() -> int:
        state["now"] += 1
        return state["now"]

    return clock


@pytest.fixture
def path(tmp_path: Path) -> Path:
    return tmp_path / "state" / "bridge" / "intents.jsonl"


def test_runtime_md_test_vector(path: Path) -> None:
    store = IntentStore(path, NAMESPACE)
    assert order().canonical_payload() == "381|no|buy|37|1450|1791475230|t-uuid"
    assert store.key_for(VECTOR_INTENT, order()) == VECTOR_UUID
    assert store.create(VECTOR_INTENT, "racer", order()).idempotency_key == VECTOR_UUID


def test_create_populates_and_persists_before_returning(path: Path) -> None:
    store = IntentStore(path, NAMESPACE, clock_ns=lambda: 5_000)
    intent = store.create(VECTOR_INTENT, "racer", order())
    assert intent.state == ops_pb2.INTENT_STATE_PENDING
    assert intent.attempts == 0
    assert (intent.created_at_ns, intent.updated_at_ns) == (5_000, 5_000)
    assert intent.side == common_pb2.SIDE_NO
    assert intent.action == common_pb2.ACTION_BUY
    assert (intent.exchange_id, intent.quantity, intent.price_pips) == ("381", 37, 1450)
    assert intent.expiration_ns == 1791475230 * 1_000_000_000
    assert (intent.tournament_id, intent.strategy) == ("t-uuid", "racer")
    assert IntentStore(path, NAMESPACE).get(VECTOR_INTENT) == intent


def test_create_is_idempotent_per_intent_id(path: Path) -> None:
    store = IntentStore(path, NAMESPACE, clock_ns=ticking_clock())
    first = store.create("i-1", "sniper", order())
    again = store.create("i-1", "sniper", order(quantity=99), idempotency_key="other")
    assert again == first
    assert len(path.read_text().splitlines()) == 1


def test_explicit_idempotency_key_is_kept(path: Path) -> None:
    store = IntentStore(path, NAMESPACE)
    assert store.create("i-1", "racer", order(), idempotency_key="bundle-key").idempotency_key == "bundle-key"


def test_update_semantics(path: Path) -> None:
    store = IntentStore(path, NAMESPACE, clock_ns=ticking_clock())
    created = store.create("i-1", "sniper", order())
    sent = store.update("i-1", ops_pb2.INTENT_STATE_SENT, bump_attempts=True)
    assert sent.state == ops_pb2.INTENT_STATE_SENT
    assert sent.attempts == 1
    assert sent.updated_at_ns > created.updated_at_ns
    assert sent.created_at_ns == created.created_at_ns
    unknown = store.update(
        "i-1", ops_pb2.INTENT_STATE_UNKNOWN, error_code="ORDER_STATUS_UNKNOWN", bump_attempts=True
    )
    assert (unknown.attempts, unknown.last_error_code) == (2, "ORDER_STATUS_UNKNOWN")
    acked = store.update("i-1", ops_pb2.INTENT_STATE_ACKED, order_id=777)
    assert (acked.order_id, acked.attempts, acked.last_error_code) == (777, 2, "ORDER_STATUS_UNKNOWN")
    assert acked.idempotency_key == created.idempotency_key
    assert len(path.read_text().splitlines()) == 4
    with pytest.raises(KeyError):
        store.update("missing", ops_pb2.INTENT_STATE_SENT)


def test_returned_messages_are_copies(path: Path) -> None:
    store = IntentStore(path, NAMESPACE)
    store.create("i-1", "sniper", order()).quantity = 1
    got = store.get("i-1")
    assert got is not None
    got.state = ops_pb2.INTENT_STATE_DONE
    assert store.get("i-1") == IntentStore(path, NAMESPACE).get("i-1")
    assert store.all()["i-1"].quantity == 37


def test_restart_sees_identical_state(path: Path) -> None:
    store = IntentStore(path, NAMESPACE, clock_ns=ticking_clock())
    store.create("a", "racer", order())
    store.create("b", "sniper", order(side="yes", action="sell", price_pips=6000))
    store.create("c", "sniper", order(exchange_id="400"))
    store.update("a", ops_pb2.INTENT_STATE_SENT, bump_attempts=True)
    store.update("b", ops_pb2.INTENT_STATE_ACKED, order_id=5)
    store.update("c", ops_pb2.INTENT_STATE_REJECTED, error_code="INSUFFICIENT_FUNDS")
    reloaded = IntentStore(path, NAMESPACE)
    assert reloaded.all() == store.all()
    assert [i.intent_id for i in reloaded.unresolved()] == ["a"]


def test_torn_final_line_is_skipped_and_terminated(path: Path) -> None:
    store = IntentStore(path, NAMESPACE, clock_ns=ticking_clock())
    store.create("a", "racer", order())
    store.update("a", ops_pb2.INTENT_STATE_SENT)
    full = path.read_bytes()
    last = full.splitlines()[-1]
    path.write_bytes(full + last[: len(last) // 2])  # crash mid-write of a third line

    reloaded = IntentStore(path, NAMESPACE, clock_ns=ticking_clock(10_000))
    assert reloaded.get("a") == store.get("a")
    reloaded.create("b", "racer", order(quantity=2))
    again = IntentStore(path, NAMESPACE)
    assert set(again.all()) == {"a", "b"}
    assert again.get("b") == reloaded.get("b")


def test_unresolved_states_and_order(path: Path) -> None:
    store = IntentStore(path, NAMESPACE, clock_ns=ticking_clock())
    for intent_id in ["p", "s", "u", "k", "r", "d"]:
        store.create(intent_id, "racer", order())
    store.update("s", ops_pb2.INTENT_STATE_SENT)
    store.update("u", ops_pb2.INTENT_STATE_UNKNOWN)
    store.update("k", ops_pb2.INTENT_STATE_ACKED)
    store.update("r", ops_pb2.INTENT_STATE_REJECTED)
    store.update("d", ops_pb2.INTENT_STATE_DONE)
    store.update("p", ops_pb2.INTENT_STATE_PENDING)  # a late update must not reorder by created time
    assert [i.intent_id for i in store.unresolved()] == ["p", "s", "u"]


def test_bundle_key_is_deterministic_and_shared(path: Path) -> None:
    store = IntentStore(path, NAMESPACE)
    legs = [order(exchange_id="381"), order(exchange_id="382", side="yes")]
    key = store.bundle_key("racer:rel-9", legs)
    assert key == IntentStore(path, NAMESPACE).bundle_key("racer:rel-9", legs)
    assert key != store.bundle_key("racer:rel-9", list(reversed(legs)))
    assert key != store.bundle_key("racer:rel-10", legs)
    assert key != store.key_for("racer:rel-9", legs[0])
    for n, leg in enumerate(legs):
        store.create(f"racer:rel-9#{n}", "racer", leg, idempotency_key=key)
    store.create("other", "racer", order())
    assert [i.intent_id for i in store.by_key(key)] == ["racer:rel-9#0", "racer:rel-9#1"]


@pytest.mark.parametrize(
    "req",
    [
        order(),
        order(side="yes", action="sell", price_pips=9950, quantity=2_147_483_647),
        order(expiration=datetime(2026, 11, 4, 16, 55, 0, 123_456, tzinfo=UTC)),
    ],
)
def test_to_request_round_trip(path: Path, req: OrderRequest) -> None:
    store = IntentStore(path, NAMESPACE)
    intent = store.create("i", "racer", req)
    rebuilt = IntentStore.to_request(intent)
    assert rebuilt.canonical_payload() == req.canonical_payload()
    assert store.key_for("i", rebuilt) == intent.idempotency_key
    assert rebuilt.expiration == datetime.fromtimestamp(req.expiration_unix_s, UTC)
    assert rebuilt.expiration.microsecond == 0
