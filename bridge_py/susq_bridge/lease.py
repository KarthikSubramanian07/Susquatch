"""Single-trader lease (``ops/state/trader.lease``): only the holder of an unexpired lease may trade.

Every read-modify-write runs under an exclusive ``flock`` on a sidecar ``.lock`` file, and the lease
itself is replaced atomically (tmp file + fsync + rename + directory fsync), so readers never see a
half-written lease and two processes can never both believe they acquired it. Takeover happens only
after the current lease expires; ``epoch`` increments whenever the holder identity changes.
"""

from __future__ import annotations

import asyncio
import fcntl
import inspect
import os
import socket
import time
from collections.abc import Awaitable, Callable, Iterator
from contextlib import contextmanager
from pathlib import Path

from google.protobuf.json_format import ParseError
from susq.contracts import from_json, ops_pb2, to_json

__all__ = ["Lease", "atomic_write"]

_NS_PER_S = 1_000_000_000


def atomic_write(path: Path, data: bytes) -> None:
    """Replace ``path`` with ``data`` so readers see either the old or the new content, never a mix."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with tmp.open("wb") as fh:
        fh.write(data)
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(tmp, path)
    dir_fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


class Lease:
    """One process's view of the shared trader lease. All decisions are made from the file."""

    def __init__(
        self,
        path: Path,
        holder: str,
        *,
        ttl_s: float,
        clock_ns: Callable[[], int] = time.time_ns,
        host: str | None = None,
        pid: int | None = None,
    ) -> None:
        self._path = path
        self._lock_path = path.with_name(path.name + ".lock")
        self._holder = holder
        self._ttl_ns = int(ttl_s * _NS_PER_S)
        self._clock_ns = clock_ns
        self._host = socket.gethostname() if host is None else host
        self._pid = os.getpid() if pid is None else pid

    def read(self) -> ops_pb2.TraderLease | None:
        """The lease on disk, or ``None`` if there is none (or it is unreadable)."""
        try:
            return from_json(ops_pb2.TraderLease, self._path.read_text())
        except (OSError, UnicodeDecodeError, ParseError):
            return None

    def acquire(self) -> bool:
        """Take the lease if it is free, expired or already ours. True if we hold it afterwards."""
        with self._locked():
            now = self._clock_ns()
            current = self.read()
            ours = current is not None and self._is_ours(current)
            if current is not None and not ours and current.expires_at_ns > now:
                return False
            lease = ops_pb2.TraderLease(
                holder=self._holder,
                host=self._host,
                pid=self._pid,
                acquired_at_ns=now,
                expires_at_ns=now + self._ttl_ns,
            )
            if current is None:
                lease.epoch = 1
            elif ours:
                lease.epoch = current.epoch
                if current.expires_at_ns > now:
                    lease.acquired_at_ns = current.acquired_at_ns
            else:
                lease.epoch = current.epoch + 1
            self._write(lease)
            return True

    def renew(self) -> bool:
        """Extend our unexpired lease by one TTL. False (and nothing written) if we no longer hold it."""
        with self._locked():
            now = self._clock_ns()
            current = self.read()
            if current is None or not self._is_ours(current) or current.expires_at_ns <= now:
                return False
            current.expires_at_ns = now + self._ttl_ns
            self._write(current)
            return True

    def release(self) -> None:
        """Expire our lease now (keeping holder/epoch) so a successor can take over immediately."""
        with self._locked():
            now = self._clock_ns()
            current = self.read()
            if current is None or not self._is_ours(current) or current.expires_at_ns <= now:
                return
            current.expires_at_ns = now
            self._write(current)

    def held(self) -> bool:
        """True if the file says we hold an unexpired lease."""
        current = self.read()
        return current is not None and self._is_ours(current) and current.expires_at_ns > self._clock_ns()

    async def keep_alive(self, renew_s: float, on_lost: Callable[[], Awaitable[None] | None]) -> None:
        """Renew every ``renew_s`` seconds; on the first failed renewal call ``on_lost`` once and return."""
        while True:
            await asyncio.sleep(renew_s)
            if not self.renew():
                result = on_lost()
                if inspect.isawaitable(result):
                    await result
                return

    def _is_ours(self, lease: ops_pb2.TraderLease) -> bool:
        return lease.holder == self._holder and lease.host == self._host and lease.pid == self._pid

    def _write(self, lease: ops_pb2.TraderLease) -> None:
        atomic_write(self._path, to_json(lease).encode())

    @contextmanager
    def _locked(self) -> Iterator[None]:
        self._lock_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(self._lock_path, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            yield
        finally:
            os.close(fd)
