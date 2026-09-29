"""Adaptive async rate limiter with a reserved order budget.

The platform publishes no rate limits, so the request rate adapts: additive increase on success,
multiplicative decrease on 429 (plus a hard pause for ``Retry-After``). The rate is split across two
token buckets. Writes (place/cancel) draw from their own bucket and may borrow idle read tokens;
reads never touch the write bucket, so a flood of book polling can never delay an order or a cancel.
"""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable
from typing import Literal

__all__ = ["AdaptiveLimiter"]

_EPS = 1e-9


class _Bucket:
    __slots__ = ("rate", "tokens")

    def __init__(self, rate: float) -> None:
        self.rate = rate
        self.tokens = self.capacity

    @property
    def capacity(self) -> float:
        return max(1.0, self.rate)

    def refill(self, dt: float) -> None:
        self.tokens = min(self.capacity, self.tokens + dt * self.rate)

    def ready(self) -> bool:
        return self.tokens >= 1.0 - _EPS

    def wait_s(self) -> float:
        return max(0.0, 1.0 - self.tokens) / self.rate


class AdaptiveLimiter:
    """AIMD token-bucket limiter shared by every REST call the bridge makes."""

    def __init__(
        self,
        initial_rps: float,
        min_rps: float,
        max_rps: float,
        order_budget_frac: float,
        *,
        increase_rps: float = 0.05,
        decrease_factor: float = 0.5,
        clock: Callable[[], float] = time.monotonic,
        sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
    ) -> None:
        if not 0 < min_rps <= initial_rps <= max_rps:
            raise ValueError(
                f"need 0 < min_rps <= initial_rps <= max_rps, got {min_rps}/{initial_rps}/{max_rps}"
            )
        if not 0 < order_budget_frac < 1:
            raise ValueError(f"order_budget_frac must be in (0, 1), got {order_budget_frac}")
        if not 0 < decrease_factor < 1:
            raise ValueError(f"decrease_factor must be in (0, 1), got {decrease_factor}")
        self._rps = initial_rps
        self._min_rps = min_rps
        self._max_rps = max_rps
        self._frac = order_budget_frac
        self._increase_rps = increase_rps
        self._decrease_factor = decrease_factor
        self._clock = clock
        self._sleep = sleep
        self._write = _Bucket(initial_rps * order_budget_frac)
        self._read = _Bucket(initial_rps * (1 - order_budget_frac))
        self._last = clock()
        self._blocked_until = float("-inf")

    @property
    def rps(self) -> float:
        return self._rps

    async def acquire(self, kind: Literal["read", "write"]) -> None:
        """Wait until a token of ``kind`` is available and no Retry-After pause is in force, then take it."""
        while True:
            now = self._refill()
            if now < self._blocked_until:
                await self._sleep(self._blocked_until - now)
                continue
            if kind == "write" and self._write.ready():
                self._write.tokens -= 1.0
                return
            if self._read.ready():
                self._read.tokens -= 1.0
                return
            wait = self._read.wait_s()
            if kind == "write":
                wait = min(wait, self._write.wait_s())
            await self._sleep(wait)

    def on_success(self) -> None:
        """Additive increase, capped at ``max_rps``."""
        self._set_rps(min(self._max_rps, self._rps + self._increase_rps))

    def on_rate_limited(self, retry_after_s: float | None) -> None:
        """Multiplicative decrease (floored at ``min_rps``) and pause every caller for Retry-After."""
        self._set_rps(max(self._min_rps, self._rps * self._decrease_factor))
        self._blocked_until = max(self._blocked_until, self._clock() + (retry_after_s or 1.0))

    def _refill(self) -> float:
        now = self._clock()
        dt = max(0.0, now - self._last)
        self._write.refill(dt)
        self._read.refill(dt)
        self._last = now
        return now

    def _set_rps(self, rps: float) -> None:
        self._refill()
        self._rps = rps
        for bucket, rate in ((self._write, rps * self._frac), (self._read, rps * (1 - self._frac))):
            bucket.rate = rate
            bucket.tokens = min(bucket.tokens, bucket.capacity)
