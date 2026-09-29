"""Append-only JSONL journal of every platform exchange (``journal/YYYY-MM-DD/<producer>.journal.jsonl``).

Each line is one ``susq.v1.JournalEntry`` in compact canonical proto3 JSON. Files rotate on the UTC day
of each entry's own timestamp. Secrets are scrubbed before anything touches disk: known secret strings
are substituted anywhere in ``path``/``body_json``, and credential-looking JSON keys are blanked
recursively. Writes (POST/DELETE) and events/errors are fsynced immediately; routine reads are fsynced
at most once per ``fsync_interval_s`` so the journal never becomes the bottleneck.
"""

from __future__ import annotations

import os
from collections.abc import Callable, Mapping, Sequence
from datetime import UTC, date, datetime, timedelta
from pathlib import Path
from typing import Any, BinaryIO

import orjson
from google.protobuf.json_format import MessageToDict
from susq.contracts import ops_pb2
from susq.platform.client import HttpRecord

__all__ = ["REDACTED", "SENSITIVE_KEYS", "Journal"]

REDACTED = "[redacted]"
SENSITIVE_KEYS = frozenset(
    {"token", "apikey", "anonkey", "authorization", "x-api-key", "access_token", "api_key"}
)
_URGENT_METHODS = frozenset({"POST", "DELETE"})
_URGENT_DIRECTIONS = frozenset({"event", "error"})
_EPOCH = datetime(1970, 1, 1, tzinfo=UTC)
_NS_PER_S = 1_000_000_000


def _utc_now() -> datetime:
    return datetime.now(UTC)


def _to_ns(ts: datetime) -> int:
    return (ts - _EPOCH) // timedelta(microseconds=1) * 1_000


def _scrub(value: Any) -> Any:
    if isinstance(value, dict):
        return {k: REDACTED if str(k).lower() in SENSITIVE_KEYS else _scrub(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_scrub(v) for v in value]
    return value


class Journal:
    """Writes ``JournalEntry`` lines for one producer, rotating by UTC day."""

    def __init__(
        self,
        root: Path,
        producer: str = "bridge",
        *,
        secrets: Sequence[str] = (),
        clock: Callable[[], datetime] = _utc_now,
        fsync_interval_s: float = 1.0,
    ) -> None:
        self._root = root
        self._producer = producer
        self._secrets = tuple(s for s in secrets if s)
        self._clock = clock
        self._fsync_interval_s = fsync_interval_s
        self._fh: BinaryIO | None = None
        self._day: date | None = None
        self._dirty = False
        self._last_fsync_s = 0.0

    def path_for(self, day: date) -> Path:
        """Journal file for a UTC day."""
        return self._root / day.isoformat() / f"{self._producer}.journal.jsonl"

    def record_http(self, rec: HttpRecord) -> None:
        """Journal one request/response/error exactly as the client reported it (after redaction)."""
        self._write(
            ops_pb2.JournalEntry(
                ts_ns=rec.ts_ns,
                producer=self._producer,
                direction=rec.direction,
                method=rec.method,
                path=rec.path,
                body_json=rec.body_json,
                status=rec.status,
                latency_us=rec.latency_us,
                request_id=rec.request_id,
                idempotency_key=rec.idempotency_key,
                correlation_id=rec.correlation_id,
            )
        )

    def event(
        self, kind: str, data: Mapping[str, Any], *, correlation_id: str = "", idempotency_key: str = ""
    ) -> None:
        """Journal a bridge-side event (state change, decision, lease/kill transition, ...)."""
        self._write(
            ops_pb2.JournalEntry(
                ts_ns=_to_ns(self._clock()),
                producer=self._producer,
                direction="event",
                method=kind,
                body_json=orjson.dumps(data, default=str).decode(),
                idempotency_key=idempotency_key,
                correlation_id=correlation_id,
            )
        )

    def flush(self, *, fsync: bool = True) -> None:
        """Flush buffered lines to the OS and, by default, to stable storage."""
        if self._fh is None:
            return
        self._fh.flush()
        if fsync and self._dirty:
            os.fsync(self._fh.fileno())
            self._dirty = False
            self._last_fsync_s = self._clock().timestamp()

    def close(self) -> None:
        """Flush, fsync and close the current file. The journal reopens lazily if written again."""
        if self._fh is None:
            return
        self.flush()
        self._fh.close()
        self._fh = None
        self._day = None

    def _redact_text(self, text: str) -> str:
        for secret in self._secrets:
            text = text.replace(secret, REDACTED)
        return text

    def _redact_body(self, body: str) -> str:
        body = self._redact_text(body)
        try:
            parsed = orjson.loads(body)
        except orjson.JSONDecodeError:
            return body
        if not isinstance(parsed, dict | list):
            return body
        return orjson.dumps(_scrub(parsed)).decode()

    def _write(self, entry: ops_pb2.JournalEntry) -> None:
        entry.path = self._redact_text(entry.path)
        entry.body_json = self._redact_body(entry.body_json)
        fh = self._file_for(datetime.fromtimestamp(entry.ts_ns // _NS_PER_S, UTC).date())
        fh.write(orjson.dumps(MessageToDict(entry)) + b"\n")
        self._dirty = True
        urgent = entry.method in _URGENT_METHODS or entry.direction in _URGENT_DIRECTIONS
        due = self._clock().timestamp() - self._last_fsync_s >= self._fsync_interval_s
        self.flush(fsync=urgent or due)

    def _file_for(self, day: date) -> BinaryIO:
        if self._fh is not None and self._day == day:
            return self._fh
        self.close()
        path = self.path_for(day)
        path.parent.mkdir(parents=True, exist_ok=True)
        self._fh = path.open("ab")
        self._day = day
        return self._fh
