"""Fault injection, a controllable clock and a per-key rate limiter for the mock exchange."""

from __future__ import annotations

import time
from collections import deque
from dataclasses import asdict, dataclass, fields
from datetime import UTC, datetime, timedelta
from typing import Any

__all__ = ["Clock", "Faults", "RateWindow"]


@dataclass
class Faults:
    """Every knob is off by default. Counters (``*_next``) decrement as they fire."""

    #: Requests per second allowed per API key (sliding 1 s window); ``None`` = unlimited.
    rate_limit_rps: float | None = None
    #: ``Retry-After`` seconds sent with a 429.
    retry_after_s: int = 1
    #: Next N order writes (/orders, /orders/batch, /orders/multi-leg) execute, are stored under their
    #: idempotency key, and then answer ``502 ORDER_STATUS_UNKNOWN``. A same-key retry replays.
    order_status_unknown_next: int = 0
    #: Next N order writes answer ``503 SERVICE_UNAVAILABLE`` *without* executing.
    service_unavailable_next: int = 0
    #: Next N order writes answer ``409 REQUEST_IN_FLIGHT`` without executing.
    request_in_flight_next: int = 0
    #: Next N cancel-all calls pause after cancelling half and answer ``503`` with details.
    cancel_all_paused_next: int = 0
    #: Next N ``DELETE /orders/{id}`` calls answer ``503 SERVICE_UNAVAILABLE`` without cancelling.
    cancel_unavailable_next: int = 0
    #: Probability that any ``/api/v1`` request answers a random ``503`` (seeded RNG).
    random_503_prob: float = 0.0
    #: Extra latency added to every ``/api/v1`` request.
    latency_ms: int = 0

    def update(self, patch: dict[str, Any]) -> None:
        known = {f.name for f in fields(self)}
        unknown = set(patch) - known
        if unknown:
            raise ValueError(f"unknown fault fields: {sorted(unknown)}")
        for key, value in patch.items():
            setattr(self, key, value)

    def take(self, name: str) -> bool:
        """Consume one shot of a ``*_next`` counter. True if it fired."""
        remaining = int(getattr(self, name))
        if remaining > 0:
            setattr(self, name, remaining - 1)
            return True
        return False

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


class Clock:
    """Wall clock plus an adjustable offset, or a frozen instant (tests)."""

    def __init__(self, frozen: datetime | None = None) -> None:
        self._frozen = frozen
        self._offset = timedelta()

    def now(self) -> datetime:
        base = self._frozen if self._frozen is not None else datetime.now(UTC)
        return base + self._offset

    def advance(self, seconds: float) -> None:
        self._offset += timedelta(seconds=seconds)

    def set(self, when: datetime) -> None:
        self._frozen = when
        self._offset = timedelta()


class RateWindow:
    """Sliding one-second window per key, on the real monotonic clock."""

    def __init__(self) -> None:
        self._hits: dict[str, deque[float]] = {}

    def allow(self, key: str, rps: float) -> bool:
        now = time.monotonic()
        hits = self._hits.setdefault(key, deque())
        while hits and now - hits[0] >= 1.0:
            hits.popleft()
        if len(hits) >= rps:
            return False
        hits.append(now)
        return True

    def reset(self) -> None:
        self._hits.clear()
