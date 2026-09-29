"""AdaptiveLimiter under virtual time: throughput, AIMD, Retry-After and the reserved write budget."""

from __future__ import annotations

import asyncio
import heapq
from collections.abc import Callable

import pytest
from hypothesis import given
from hypothesis import strategies as st
from susq_bridge.limiter import AdaptiveLimiter


class VirtualTime:
    """A clock plus a sleep that only returns when the test advances time past its deadline."""

    def __init__(self) -> None:
        self.now = 0.0
        self._timers: list[tuple[float, int, asyncio.Future[None]]] = []
        self._seq = 0

    def clock(self) -> float:
        return self.now

    async def sleep(self, dt: float) -> None:
        fut: asyncio.Future[None] = asyncio.get_running_loop().create_future()
        heapq.heappush(self._timers, (self.now + max(dt, 0.0), self._seq, fut))
        self._seq += 1
        await fut

    async def run_until(self, t: float) -> None:
        """Advance to ``t``, waking every sleeper whose deadline passes, in deadline order."""
        while True:
            await _settle()
            if not self._timers or self._timers[0][0] > t:
                self.now = max(self.now, t)
                await _settle()
                return
            deadline, _, fut = heapq.heappop(self._timers)
            self.now = max(self.now, deadline)
            if not fut.cancelled():
                fut.set_result(None)


async def _settle() -> None:
    for _ in range(10):
        await asyncio.sleep(0)


def make(vt: VirtualTime, **overrides: float) -> AdaptiveLimiter:
    params: dict[str, float] = {
        "initial_rps": 4.0,
        "min_rps": 0.5,
        "max_rps": 20.0,
        "order_budget_frac": 0.35,
    }
    params.update(overrides)
    return AdaptiveLimiter(**params, clock=vt.clock, sleep=vt.sleep)  # type: ignore[arg-type]


def spawn_loop(lim: AdaptiveLimiter, kind: str, done: list[float], vt: VirtualTime) -> asyncio.Task[None]:
    async def run() -> None:
        while True:
            await lim.acquire(kind)  # type: ignore[arg-type]
            done.append(vt.now)

    return asyncio.create_task(run())


async def stop(*tasks: asyncio.Task[None]) -> None:
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)


def rate_between(times: list[float], start: float, end: float) -> float:
    return sum(start <= t < end for t in times) / (end - start)


async def test_steady_state_read_throughput_matches_read_share() -> None:
    vt = VirtualTime()
    lim = make(vt)
    reads: list[float] = []
    task = spawn_loop(lim, "read", reads, vt)
    await vt.run_until(200.0)
    await stop(task)
    assert rate_between(reads, 10.0, 200.0) == pytest.approx(4.0 * 0.65, rel=0.02)


async def test_writes_alone_may_borrow_idle_read_budget() -> None:
    vt = VirtualTime()
    lim = make(vt)
    writes: list[float] = []
    task = spawn_loop(lim, "write", writes, vt)
    await vt.run_until(200.0)
    await stop(task)
    assert rate_between(writes, 10.0, 200.0) == pytest.approx(4.0, rel=0.02)


async def test_saturated_reads_cannot_delay_a_write() -> None:
    vt = VirtualTime()
    lim = make(vt)
    reads: list[float] = []
    readers = [spawn_loop(lim, "read", reads, vt) for _ in range(20)]
    await vt.run_until(30.0)

    wrote: list[float] = []
    for _ in range(3):
        writer = spawn_loop(lim, "write", wrote, vt)
        await vt.run_until(vt.now + 0.001)
        await stop(writer)
    # The write bucket (capacity 1.4) sat untouched while reads were saturated.
    assert wrote == [pytest.approx(30.0, abs=0.01)]

    writer = spawn_loop(lim, "write", wrote, vt)
    await vt.run_until(90.0)
    await stop(writer, *readers)
    assert rate_between(wrote, 40.0, 90.0) == pytest.approx(4.0 * 0.35, rel=0.05)
    assert rate_between(reads, 40.0, 90.0) == pytest.approx(4.0 * 0.65, rel=0.05)


async def test_rate_limited_halves_rps_and_blocks_until_retry_after() -> None:
    vt = VirtualTime()
    lim = make(vt)
    lim.on_rate_limited(3.0)
    assert lim.rps == 2.0

    done: list[float] = []
    write = asyncio.create_task(lim.acquire("write"))
    write.add_done_callback(lambda _: done.append(vt.now))
    await vt.run_until(2.99)
    assert not done
    await vt.run_until(3.5)
    assert done == [pytest.approx(3.0)]

    reads: list[float] = []
    task = spawn_loop(lim, "read", reads, vt)
    await vt.run_until(200.0)
    await stop(task)
    assert rate_between(reads, 10.0, 200.0) == pytest.approx(2.0 * 0.65, rel=0.02)


async def test_retry_after_defaults_to_one_second() -> None:
    vt = VirtualTime()
    lim = make(vt)
    lim.on_rate_limited(None)
    done: list[float] = []
    task = asyncio.create_task(lim.acquire("read"))
    task.add_done_callback(lambda _: done.append(vt.now))
    await vt.run_until(5.0)
    assert done == [pytest.approx(1.0)]


async def test_block_never_shortens() -> None:
    vt = VirtualTime()
    lim = make(vt)
    lim.on_rate_limited(10.0)
    lim.on_rate_limited(2.0)
    done: list[float] = []
    task = asyncio.create_task(lim.acquire("write"))
    task.add_done_callback(lambda _: done.append(vt.now))
    await vt.run_until(20.0)
    assert done == [pytest.approx(10.0)]


def test_additive_increase_is_capped() -> None:
    vt = VirtualTime()
    lim = make(vt, initial_rps=19.9, increase_rps=0.05)
    lim.on_success()
    assert lim.rps == pytest.approx(19.95)
    for _ in range(10):
        lim.on_success()
    assert lim.rps == 20.0


def test_multiplicative_decrease_has_a_floor() -> None:
    vt = VirtualTime()
    lim = make(vt)
    for _ in range(10):
        lim.on_rate_limited(0.0)
    assert lim.rps == 0.5


@pytest.mark.parametrize(
    "kwargs",
    [
        {"initial_rps": 30.0},
        {"initial_rps": 0.1},
        {"min_rps": 0.0, "initial_rps": 1.0},
        {"order_budget_frac": 1.0},
        {"order_budget_frac": 0.0},
        {"decrease_factor": 1.0},
    ],
)
def test_rejects_bad_parameters(kwargs: dict[str, float]) -> None:
    with pytest.raises(ValueError):  # noqa: PT011 - message varies by parameter
        make(VirtualTime(), **kwargs)


events = st.lists(st.one_of(st.just("ok"), st.none(), st.floats(0.0, 120.0)), max_size=200)


@given(
    initial=st.floats(0.5, 20.0),
    increase=st.floats(0.001, 5.0),
    decrease=st.floats(0.01, 0.99),
    seq=events,
)
def test_rps_stays_within_bounds(initial: float, increase: float, decrease: float, seq: list[object]) -> None:
    vt = VirtualTime()
    lim = make(vt, initial_rps=initial, increase_rps=increase, decrease_factor=decrease)
    apply: dict[bool, Callable[[object], None]] = {
        True: lambda _: lim.on_success(),
        False: lambda ev: lim.on_rate_limited(ev),  # type: ignore[arg-type]
    }
    for ev in seq:
        apply[ev == "ok"](ev)
        vt.now += 0.1
        assert 0.5 <= lim.rps <= 20.0
