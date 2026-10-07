"""Trader lease: single-holder semantics, in-process with an injected clock and across real OS processes."""

from __future__ import annotations

import asyncio
import json
import subprocess
import sys
import time
from pathlib import Path

import pytest
from susq.contracts import ops_pb2, to_json
from susq_bridge.lease import Lease

S = 1_000_000_000


class Clock:
    def __init__(self) -> None:
        self.now = 1_000 * S

    def __call__(self) -> int:
        return self.now


@pytest.fixture
def path(tmp_path: Path) -> Path:
    return tmp_path / "ops" / "state" / "trader.lease"


@pytest.fixture
def clock() -> Clock:
    return Clock()


def lease(path: Path, clock: Clock, *, holder: str = "bridge", pid: int = 1, ttl_s: float = 30) -> Lease:
    return Lease(path, holder, ttl_s=ttl_s, clock_ns=clock, host="h", pid=pid)


# --- in-process, deterministic clock -------------------------------------------------------------------


def test_first_acquire_writes_epoch_one(path: Path, clock: Clock) -> None:
    a = lease(path, clock)
    assert a.read() is None
    assert a.acquire()
    current = a.read()
    assert current is not None
    assert (current.holder, current.host, current.pid, current.epoch) == ("bridge", "h", 1, 1)
    assert (current.acquired_at_ns, current.expires_at_ns) == (clock.now, clock.now + 30 * S)
    assert a.held()


def test_no_takeover_before_expiry_then_takeover_bumps_epoch(path: Path, clock: Clock) -> None:
    a, b = lease(path, clock), lease(path, clock, holder="rust-live", pid=2)
    assert a.acquire()
    clock.now += 29 * S
    assert not b.acquire()
    assert not b.renew()
    clock.now += 1 * S
    assert not a.held()
    assert b.acquire()
    current = b.read()
    assert current is not None
    assert (current.holder, current.epoch, current.acquired_at_ns) == ("rust-live", 2, clock.now)
    assert not a.renew()
    assert not a.acquire()


def test_renew_extends_without_changing_epoch(path: Path, clock: Clock) -> None:
    a = lease(path, clock)
    assert a.acquire()
    acquired = clock.now
    for _ in range(5):
        clock.now += 5 * S
        assert a.renew()
    current = a.read()
    assert current is not None
    assert (current.epoch, current.acquired_at_ns, current.expires_at_ns) == (1, acquired, clock.now + 30 * S)
    clock.now += 5 * S
    assert a.acquire()  # re-acquire while held behaves like a renew
    current = a.read()
    assert current is not None
    assert (current.epoch, current.acquired_at_ns) == (1, acquired)


def test_renew_after_own_expiry_fails(path: Path, clock: Clock) -> None:
    a = lease(path, clock)
    assert a.acquire()
    clock.now += 31 * S
    assert not a.renew()
    assert a.acquire()
    current = a.read()
    assert current is not None
    assert (current.epoch, current.acquired_at_ns) == (1, clock.now)


def test_release_hands_over_immediately(path: Path, clock: Clock) -> None:
    a, b = lease(path, clock), lease(path, clock, pid=2)
    assert a.acquire()
    b.release()  # not ours: no effect
    assert a.held()
    a.release()
    released = a.read()
    assert released is not None
    assert (released.pid, released.epoch, released.expires_at_ns) == (1, 1, clock.now)
    assert not a.held()
    assert b.acquire()
    current = b.read()
    assert current is not None
    assert (current.pid, current.epoch) == (2, 2)


def test_identity_includes_host_and_pid(path: Path, clock: Clock) -> None:
    assert lease(path, clock, pid=1).acquire()
    assert not lease(path, clock, pid=2).acquire()
    assert not Lease(path, "bridge", ttl_s=30, clock_ns=clock, host="other", pid=1).acquire()


def test_held_reads_the_file_not_memory(path: Path, clock: Clock) -> None:
    a = lease(path, clock)
    assert a.acquire()
    path.write_text(
        to_json(ops_pb2.TraderLease(holder="rust-live", host="h", pid=9, expires_at_ns=clock.now + S))
    )
    assert not a.held()
    path.write_text("{torn")
    assert a.read() is None
    assert not a.held()
    assert a.acquire()  # unreadable lease counts as no lease
    path.unlink()
    assert not a.held()


def test_defaults_to_this_host_and_pid(path: Path) -> None:
    import os
    import socket

    a = Lease(path, "bridge", ttl_s=30)
    assert a.acquire()
    current = a.read()
    assert current is not None
    assert (current.host, current.pid) == (socket.gethostname(), os.getpid())


async def test_keep_alive_renews_until_lost(path: Path) -> None:
    a = Lease(path, "bridge", ttl_s=5, host="h", pid=1)
    assert a.acquire()
    first = a.read()
    lost = asyncio.Event()
    calls: list[int] = []

    async def on_lost() -> None:
        calls.append(1)
        lost.set()

    task = asyncio.create_task(a.keep_alive(0.01, on_lost))
    await asyncio.sleep(0.05)
    renewed = a.read()
    assert first is not None
    assert renewed is not None
    assert renewed.expires_at_ns > first.expires_at_ns
    path.write_text(to_json(ops_pb2.TraderLease(holder="thief", expires_at_ns=time.time_ns() + 60 * S)))
    await asyncio.wait_for(lost.wait(), timeout=2)
    await asyncio.wait_for(task, timeout=2)
    assert calls == [1]


async def test_keep_alive_accepts_sync_callback(path: Path) -> None:
    a = Lease(path, "bridge", ttl_s=5, host="h", pid=1)
    calls: list[int] = []
    await asyncio.wait_for(a.keep_alive(0.01, lambda: calls.append(1)), timeout=2)  # never acquired
    assert calls == [1]


# --- real OS processes ----------------------------------------------------------------------------------
# Children are separate interpreters (``python -c``) importing the installed package, so the test runs
# the exact code a second bridge or rust-live process would, with its own pid and its own flock.

CHILD_ACQUIRE = """
import json, sys
from pathlib import Path
from susq_bridge.lease import Lease
lease = Lease(Path(sys.argv[1]), "rust-live", ttl_s=float(sys.argv[2]))
won = lease.acquire()
cur = lease.read()
print(json.dumps([won, cur.epoch if cur else 0, cur.pid if cur else 0]))
"""

CHILD_RACE = """
import sys, time
from pathlib import Path
from susq_bridge.lease import Lease
lease = Lease(Path(sys.argv[1]), "racer", ttl_s=30)
go = Path(sys.argv[2])
while not go.exists():
    time.sleep(0.001)
print(int(lease.acquire()))
"""


def run_child(path: Path, ttl_s: float) -> tuple[bool, int, int]:
    proc = subprocess.run(
        [sys.executable, "-c", CHILD_ACQUIRE, str(path), str(ttl_s)],
        capture_output=True,
        text=True,
        timeout=60,
        check=True,
    )
    won, epoch, pid = json.loads(proc.stdout)
    return bool(won), int(epoch), int(pid)


def test_two_processes_contend(path: Path) -> None:
    a = Lease(path, "bridge", ttl_s=30)
    assert a.acquire()

    won, epoch, _ = run_child(path, ttl_s=30)
    assert (won, epoch) == (False, 1)
    assert a.held()

    short = Lease(path, "bridge", ttl_s=0.3)
    assert short.acquire()  # same identity: shortens our own lease
    time.sleep(0.4)
    assert not a.held()
    won, epoch, _ = run_child(path, ttl_s=0.5)
    assert (won, epoch) == (True, 2)
    assert not a.renew()
    time.sleep(0.6)

    assert a.acquire()
    assert a.read().epoch == 3  # type: ignore[union-attr]
    a.release()
    won, epoch, _ = run_child(path, ttl_s=30)
    assert (won, epoch) == (True, 4)
    assert not a.held()
    assert not a.renew()


def test_eight_processes_race_one_winner(path: Path, tmp_path: Path) -> None:
    go = tmp_path / "go"
    procs = [
        subprocess.Popen(
            [sys.executable, "-c", CHILD_RACE, str(path), str(go)], stdout=subprocess.PIPE, text=True
        )
        for _ in range(8)
    ]
    time.sleep(1.0)  # let every interpreter start and reach the barrier
    go.touch()
    results = []
    for proc in procs:
        out, _ = proc.communicate(timeout=60)
        assert proc.returncode == 0
        results.append(int(out.strip()))
    assert sorted(results) == [0] * 7 + [1]
    final = Lease(path, "racer", ttl_s=30).read()
    assert final is not None
    assert final.epoch == 1


async def test_hold_regains_an_expired_lease(path: Path) -> None:
    # Margins wide enough for a slow CI runner: a 50 ms TTL lapsed again between the renewal and the
    # `held()` check when the runner stalled (seen on PR #74's CI).
    a = Lease(path, "bridge", ttl_s=0.5, host="h", pid=1)
    assert a.acquire()
    events: list[str] = []
    regained = asyncio.Event()

    def on_regained() -> None:
        events.append("regained")
        regained.set()

    await asyncio.sleep(0.6)  # the lease lapses before the holder starts renewing (a slow open)
    assert not a.held()
    task = asyncio.create_task(a.hold(0.05, lambda: events.append("lost"), on_regained))
    await asyncio.wait_for(regained.wait(), timeout=2)
    assert a.held()
    assert events == ["regained"]
    task.cancel()


async def test_hold_never_displaces_a_live_holder(path: Path) -> None:
    a = Lease(path, "bridge", ttl_s=5, host="h", pid=1)
    assert a.acquire()
    events: list[str] = []
    task = asyncio.create_task(a.hold(0.01, lambda: events.append("lost"), lambda: events.append("regained")))
    await asyncio.sleep(0.03)
    path.write_text(to_json(ops_pb2.TraderLease(holder="thief", expires_at_ns=time.time_ns() + 60 * S)))
    await asyncio.sleep(0.1)
    assert events == ["lost"]
    assert not a.held()
    path.write_text(to_json(ops_pb2.TraderLease(holder="thief", expires_at_ns=time.time_ns() - S)))
    await asyncio.sleep(0.1)
    assert events == ["lost", "regained"]
    assert a.held()
    task.cancel()
