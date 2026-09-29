"""Journal: rotation by UTC day, redaction, parse-back, and the fsync policy."""

from __future__ import annotations

import json
import os
from datetime import UTC, date, datetime, timedelta
from pathlib import Path

import pytest
from susq.contracts import from_json, ops_pb2
from susq.platform.client import HttpRecord
from susq_bridge.journal import REDACTED, Journal

API_KEY = "mock-key-journal-0123456789abcdef"


class FakeClock:
    def __init__(self, start: datetime) -> None:
        self.now = start

    def __call__(self) -> datetime:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += timedelta(seconds=seconds)


def ns(ts: datetime) -> int:
    return int(ts.timestamp()) * 1_000_000_000 + ts.microsecond * 1_000


def rec(ts: datetime, *, method: str = "GET", path: str = "/markets", body: str = "{}") -> HttpRecord:
    return HttpRecord(
        ts_ns=ns(ts),
        direction="request",
        method=method,
        path=path,
        body_json=body,
        status=0,
        latency_us=0,
        request_id="",
        idempotency_key="",
        correlation_id="c-1",
    )


def lines(path: Path) -> list[ops_pb2.JournalEntry]:
    return [from_json(ops_pb2.JournalEntry, line) for line in path.read_text().splitlines()]


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock(datetime(2026, 10, 1, 23, 59, 59, 500_000, tzinfo=UTC))


def test_path_layout(tmp_path: Path) -> None:
    journal = Journal(tmp_path, "rust-live")
    assert journal.path_for(date(2026, 10, 1)) == tmp_path / "2026-10-01" / "rust-live.journal.jsonl"


def test_http_record_round_trips_every_field(tmp_path: Path, clock: FakeClock) -> None:
    journal = Journal(tmp_path, clock=clock)
    record = HttpRecord(
        ts_ns=ns(clock.now),
        direction="response",
        method="POST",
        path="/orders",
        body_json='{"orderId":42}',
        status=200,
        latency_us=1234,
        request_id="req-9",
        idempotency_key="k-1",
        correlation_id="c-7",
    )
    journal.record_http(record)
    journal.close()
    (entry,) = lines(journal.path_for(clock.now.date()))
    assert entry.ts_ns == record.ts_ns
    assert entry.producer == "bridge"
    assert (entry.direction, entry.method, entry.path) == ("response", "POST", "/orders")
    assert json.loads(entry.body_json) == {"orderId": 42}
    assert (entry.status, entry.latency_us, entry.request_id) == (200, 1234, "req-9")
    assert (entry.idempotency_key, entry.correlation_id) == ("k-1", "c-7")


def test_lines_are_compact_canonical_json(tmp_path: Path, clock: FakeClock) -> None:
    journal = Journal(tmp_path, clock=clock)
    journal.record_http(rec(clock.now))
    journal.close()
    (line,) = journal.path_for(clock.now.date()).read_text().splitlines()
    assert " " not in line
    parsed = json.loads(line)
    assert parsed["tsNs"] == str(ns(clock.now))  # int64 as string, lowerCamelCase
    assert "status" not in parsed  # proto3 defaults omitted


def test_rotates_across_utc_midnight(tmp_path: Path, clock: FakeClock) -> None:
    journal = Journal(tmp_path, clock=clock)
    journal.record_http(rec(clock.now, path="/before"))
    journal.event("tick", {"n": 1})
    clock.advance(1.0)
    journal.record_http(rec(clock.now, path="/after"))
    journal.event("tick", {"n": 2})
    journal.close()

    day1 = lines(journal.path_for(date(2026, 10, 1)))
    day2 = lines(journal.path_for(date(2026, 10, 2)))
    assert [e.path or e.body_json for e in day1] == ["/before", '{"n":1}']
    assert [e.path or e.body_json for e in day2] == ["/after", '{"n":2}']


def test_reopens_in_append_mode(tmp_path: Path, clock: FakeClock) -> None:
    Journal(tmp_path, clock=clock).record_http(rec(clock.now, path="/a"))
    second = Journal(tmp_path, clock=clock)
    second.record_http(rec(clock.now, path="/b"))
    second.close()
    assert [e.path for e in lines(second.path_for(clock.now.date()))] == ["/a", "/b"]


def test_event_fields(tmp_path: Path, clock: FakeClock) -> None:
    journal = Journal(tmp_path, clock=clock)
    journal.event("lease_acquired", {"epoch": 3}, correlation_id="c-2", idempotency_key="k-2")
    journal.close()
    (entry,) = lines(journal.path_for(clock.now.date()))
    assert (entry.direction, entry.method) == ("event", "lease_acquired")
    assert entry.ts_ns == ns(clock.now)
    assert entry.body_json == '{"epoch":3}'
    assert (entry.correlation_id, entry.idempotency_key) == ("c-2", "k-2")


def test_redacts_api_key_in_path_and_raw_body(tmp_path: Path, clock: FakeClock) -> None:
    journal = Journal(tmp_path, clock=clock, secrets=[API_KEY, ""])
    journal.record_http(rec(clock.now, path=f"/markets?key={API_KEY}", body=f"oops {API_KEY} not json"))
    journal.record_http(rec(clock.now, body=json.dumps({"note": f"Bearer {API_KEY}"})))
    journal.close()
    raw = journal.path_for(clock.now.date()).read_text()
    assert API_KEY not in raw
    first, second = lines(journal.path_for(clock.now.date()))
    assert first.path == f"/markets?key={REDACTED}"
    assert first.body_json == f"oops {REDACTED} not json"
    assert json.loads(second.body_json) == {"note": f"Bearer {REDACTED}"}


def test_redacts_token_fields_recursively(tmp_path: Path, clock: FakeClock) -> None:
    body = {
        "token": "rt-abc",
        "data": {
            "Authorization": "Bearer xyz",
            "items": [{"access_token": "at-1", "keep": 1}, {"API_KEY": "k", "x-api-key": "k2"}],
        },
        "anonKey": "anon",
        "apiKey": "ak",
        "orderId": 7,
    }
    journal = Journal(tmp_path, clock=clock)
    journal.record_http(rec(clock.now, body=json.dumps(body, indent=2)))
    journal.close()
    (entry,) = lines(journal.path_for(clock.now.date()))
    assert "\n" not in entry.body_json
    assert json.loads(entry.body_json) == {
        "token": REDACTED,
        "data": {
            "Authorization": REDACTED,
            "items": [{"access_token": REDACTED, "keep": 1}, {"API_KEY": REDACTED, "x-api-key": REDACTED}],
        },
        "anonKey": REDACTED,
        "apiKey": REDACTED,
        "orderId": 7,
    }


def test_events_are_redacted_too(tmp_path: Path, clock: FakeClock) -> None:
    journal = Journal(tmp_path, clock=clock, secrets=[API_KEY])
    journal.event("config", {"api_key": API_KEY, "nested": {"url": f"https://x/?k={API_KEY}"}})
    journal.close()
    assert API_KEY not in journal.path_for(clock.now.date()).read_text()


class FsyncCounter:
    def __init__(self) -> None:
        self.calls = 0
        self._real = os.fsync

    def __call__(self, fd: int) -> None:
        self.calls += 1
        self._real(fd)


@pytest.fixture
def fsyncs(monkeypatch: pytest.MonkeyPatch) -> FsyncCounter:
    counter = FsyncCounter()
    monkeypatch.setattr(os, "fsync", counter)
    return counter


def test_writes_and_events_fsync_immediately(tmp_path: Path, clock: FakeClock, fsyncs: FsyncCounter) -> None:
    journal = Journal(tmp_path, clock=clock, fsync_interval_s=60.0)
    journal.record_http(rec(clock.now))  # first GET syncs: nothing synced yet
    base = fsyncs.calls
    journal.record_http(rec(clock.now, method="POST", path="/orders"))
    journal.record_http(rec(clock.now, method="DELETE", path="/orders/1"))
    journal.event("kill", {"reason": "test"})
    assert fsyncs.calls == base + 3


def test_reads_fsync_at_most_once_per_interval(tmp_path: Path, fsyncs: FsyncCounter) -> None:
    clock = FakeClock(datetime(2026, 10, 1, 12, tzinfo=UTC))
    journal = Journal(tmp_path, clock=clock, fsync_interval_s=1.0)
    journal.record_http(rec(clock.now))
    base = fsyncs.calls
    for _ in range(20):
        clock.advance(0.01)
        journal.record_http(rec(clock.now))
    assert fsyncs.calls == base
    clock.advance(1.0)
    journal.record_http(rec(clock.now))
    assert fsyncs.calls == base + 1
    journal.flush()
    assert fsyncs.calls == base + 1  # nothing new to sync
