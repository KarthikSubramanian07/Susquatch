"""Write-ahead store of ``susq.v1.OrderIntent`` records (``state/<producer>/intents.jsonl``).

An intent is persisted (write + fsync) **before** its order is sent, so a crash between "decided" and
"acknowledged" restarts with the same idempotency key instead of minting a new one and double-placing.
The file is append-only: every create/update writes a full intent line, and the last line per
``intent_id`` wins on load. A torn final line from a crash mid-write is skipped.
"""

from __future__ import annotations

import os
import time
from collections.abc import Callable, Sequence
from datetime import UTC, datetime, timedelta
from pathlib import Path

import orjson
from google.protobuf.json_format import MessageToDict, ParseError
from susq.contracts import common_pb2, from_json, idempotency_key, ops_pb2
from susq.platform.models import OrderRequest

__all__ = ["TERMINAL", "UNRESOLVED", "IntentStore"]

#: Closed out; nothing left to do for these.
TERMINAL = frozenset({ops_pb2.INTENT_STATE_REJECTED, ops_pb2.INTENT_STATE_DONE})
#: Outcome not yet known: must be retried with the same key or reconciled against REST on restart.
UNRESOLVED = frozenset(
    {ops_pb2.INTENT_STATE_PENDING, ops_pb2.INTENT_STATE_SENT, ops_pb2.INTENT_STATE_UNKNOWN}
)

_EPOCH = datetime(1970, 1, 1, tzinfo=UTC)
_NS_PER_S = 1_000_000_000
_SIDES = {"yes": common_pb2.SIDE_YES, "no": common_pb2.SIDE_NO}
_ACTIONS = {"buy": common_pb2.ACTION_BUY, "sell": common_pb2.ACTION_SELL}
_SIDE_NAMES = {v: k for k, v in _SIDES.items()}
_ACTION_NAMES = {v: k for k, v in _ACTIONS.items()}


def _copy(intent: ops_pb2.OrderIntent) -> ops_pb2.OrderIntent:
    clone = ops_pb2.OrderIntent()
    clone.CopyFrom(intent)
    return clone


class IntentStore:
    """Durable map of ``intent_id`` → latest ``OrderIntent``. Returned messages are copies."""

    def __init__(self, path: Path, namespace: str, *, clock_ns: Callable[[], int] = time.time_ns) -> None:
        self._path = path
        self._namespace = namespace
        self._clock_ns = clock_ns
        self._intents: dict[str, ops_pb2.OrderIntent] = {}
        self._load()

    def key_for(self, intent_id: str, order: OrderRequest) -> str:
        """Deterministic idempotency key for a single order (docs/RUNTIME.md rule 4)."""
        return idempotency_key(self._namespace, intent_id, order.canonical_payload())

    def bundle_key(self, intent_id: str, legs: Sequence[OrderRequest]) -> str:
        """Deterministic idempotency key for a batch/multi-leg request, over all legs in order."""
        return idempotency_key(self._namespace, intent_id, ";".join(leg.canonical_payload() for leg in legs))

    def create(
        self, intent_id: str, strategy: str, order: OrderRequest, *, idempotency_key: str | None = None
    ) -> ops_pb2.OrderIntent:
        """Persist a PENDING intent before sending. An existing ``intent_id`` is returned unchanged."""
        if existing := self._intents.get(intent_id):
            return _copy(existing)
        now = self._clock_ns()
        intent = ops_pb2.OrderIntent(
            intent_id=intent_id,
            idempotency_key=idempotency_key or self.key_for(intent_id, order),
            strategy=strategy,
            exchange_id=order.exchange_id,
            side=_SIDES[order.side],
            action=_ACTIONS[order.action],
            quantity=order.quantity,
            price_pips=order.price_pips,
            expiration_ns=(order.expiration - _EPOCH) // timedelta(microseconds=1) * 1_000,
            tournament_id=order.tournament_id,
            state=ops_pb2.INTENT_STATE_PENDING,
            created_at_ns=now,
            updated_at_ns=now,
        )
        self._append(intent)
        return _copy(intent)

    def update(
        self,
        intent_id: str,
        state: int,
        *,
        order_id: int | None = None,
        error_code: str | None = None,
        bump_attempts: bool = False,
    ) -> ops_pb2.OrderIntent:
        """Record a state transition durably. Raises ``KeyError`` for an unknown intent."""
        intent = _copy(self._intents[intent_id])
        intent.state = state  # type: ignore[assignment]
        if order_id is not None:
            intent.order_id = order_id
        if error_code is not None:
            intent.last_error_code = error_code
        if bump_attempts:
            intent.attempts += 1
        intent.updated_at_ns = self._clock_ns()
        self._append(intent)
        return _copy(intent)

    def get(self, intent_id: str) -> ops_pb2.OrderIntent | None:
        intent = self._intents.get(intent_id)
        return None if intent is None else _copy(intent)

    def all(self) -> dict[str, ops_pb2.OrderIntent]:
        return {k: _copy(v) for k, v in self._intents.items()}

    def unresolved(self) -> list[ops_pb2.OrderIntent]:
        """PENDING, SENT and UNKNOWN intents, oldest first."""
        pending = [i for i in self._intents.values() if i.state in UNRESOLVED]
        return [_copy(i) for i in sorted(pending, key=lambda i: (i.created_at_ns, i.intent_id))]

    def by_key(self, idempotency_key: str) -> list[ops_pb2.OrderIntent]:
        """Every intent sharing ``idempotency_key`` (the legs of one bundle), ordered by ``intent_id``."""
        legs = [i for i in self._intents.values() if i.idempotency_key == idempotency_key]
        return [_copy(i) for i in sorted(legs, key=lambda i: i.intent_id)]

    @staticmethod
    def to_request(intent: ops_pb2.OrderIntent) -> OrderRequest:
        """Rebuild the exact ``OrderRequest`` (same canonical payload, hence same key) from an intent."""
        return OrderRequest(
            exchange_id=intent.exchange_id,
            side=_SIDE_NAMES[intent.side],  # type: ignore[arg-type]
            action=_ACTION_NAMES[intent.action],  # type: ignore[arg-type]
            quantity=intent.quantity,
            price_pips=intent.price_pips,
            expiration=datetime.fromtimestamp(intent.expiration_ns // _NS_PER_S, UTC),
            tournament_id=intent.tournament_id,
        )

    def _append(self, intent: ops_pb2.OrderIntent) -> None:
        self._path.parent.mkdir(parents=True, exist_ok=True)
        with self._path.open("ab") as fh:
            fh.write(orjson.dumps(MessageToDict(intent)) + b"\n")
            fh.flush()
            os.fsync(fh.fileno())
        self._intents[intent.intent_id] = intent

    def _load(self) -> None:
        try:
            raw = self._path.read_bytes()
        except FileNotFoundError:
            return
        for line in raw.splitlines():
            if not line.strip():
                continue
            try:
                intent = from_json(ops_pb2.OrderIntent, line.decode())
            except (ParseError, UnicodeDecodeError):
                continue
            if intent.intent_id:
                self._intents[intent.intent_id] = intent
        if raw and not raw.endswith(b"\n"):
            # Terminate a torn final line so the next append starts on a clean line.
            with self._path.open("ab") as fh:
                fh.write(b"\n")
                fh.flush()
                os.fsync(fh.fileno())
