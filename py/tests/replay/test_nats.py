"""Publish a replay through a real local `nats-server` (skipped when the binary is absent)."""

from __future__ import annotations

import asyncio
import shutil
import socket
import subprocess
import time

import pytest
from susq.replay.closing import StateClosing
from susq.replay.history import ContestHistory
from susq.replay.runner import SUBJECT, ChaosConfig, InvalidPayloadError, decode_batch, replay_to_nats
from susq.replay.timeline import BuildConfig, RaceSpec, build_replay

NATS = shutil.which("nats-server")
pytestmark = pytest.mark.skipif(NATS is None, reason="nats-server not installed")


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


@pytest.fixture
def nats_url():
    port = _free_port()
    proc = subprocess.Popen(
        [NATS, "-a", "127.0.0.1", "-p", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                break
        time.sleep(0.05)
    yield f"nats://127.0.0.1:{port}"
    proc.terminate()
    proc.wait(timeout=5)


async def test_replay_reaches_subscribers(
    nats_url: str, nh_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    import nats

    batches, _ = build_replay([RaceSpec("R", nh_senate)], closings, BuildConfig(seed=2))
    got: list[bytes] = []
    sub_nc = await nats.connect(servers=[nats_url])

    async def on_msg(msg) -> None:
        got.append(msg.data)

    await sub_nc.subscribe(SUBJECT, cb=on_msg)
    await sub_nc.flush()
    chaos = ChaosConfig(malformed_every=4, kill_at_s=1800, restart_after_s=300)
    stats = await replay_to_nats(batches, nats_url, speed=0.0, chaos=chaos)
    await asyncio.sleep(0.3)
    await sub_nc.drain()

    assert stats.reconnects == 1
    assert len(got) == stats.published + stats.malformed
    valid, rejected = [], 0
    for data in got:
        try:
            valid.append(decode_batch(data))
        except InvalidPayloadError:
            rejected += 1
    assert rejected == stats.malformed > 0
    assert len(valid) == stats.published
    # Timestamps were re-anchored onto the wall clock.
    now = time.time_ns()
    assert all(abs(b.header.produced_at_ns - now) < 60 * 1_000_000_000 for b in valid)
    seqs = [b.header.seq for b in valid]
    assert seqs.count(1) == 2, "fresh seq after the publisher restart"
