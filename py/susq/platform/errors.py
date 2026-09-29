"""Platform errors and the retry classification every caller branches on.

The classification is a direct transcription of the "Error envelope" table in ``docs/API.md``,
which in turn follows the official reference's "Rate limits & retries" section:

* ``retry_same_key`` - 429 ``RATE_LIMITED``, 409 ``REQUEST_IN_FLIGHT`` (wait >= 90 s),
  502 ``ORDER_STATUS_UNKNOWN``, 503 ``TX_CONFLICT`` / ``SERVICE_UNAVAILABLE``. Safe to retry a write
  with the **same** idempotency key.
* ``terminal`` - every other 4xx. Never retried automatically.
* ``retry_read`` - other 5xx (``INTERNAL_ERROR``, ``DATABASE_ERROR``, gateway errors). Reads may be
  retried; writes must reconcile first and then retry with the same key.
* ``network`` - no HTTP response at all (connect error, timeout, reset). For a write the outcome is
  unknown, exactly like a 502; for a read it is safe to retry.
"""

from __future__ import annotations

from enum import StrEnum
from typing import Any

__all__ = [
    "REQUEST_IN_FLIGHT_WAIT_S",
    "ApiError",
    "NetworkError",
    "PlatformError",
    "RetryClass",
    "classify",
]

#: The official docs: "retry the identical request with the same key after 90 seconds".
REQUEST_IN_FLIGHT_WAIT_S = 90.0

_RETRY_SAME_KEY: frozenset[tuple[int, str]] = frozenset(
    {
        (429, "RATE_LIMITED"),
        (409, "REQUEST_IN_FLIGHT"),
        (502, "ORDER_STATUS_UNKNOWN"),
        (503, "TX_CONFLICT"),
        (503, "SERVICE_UNAVAILABLE"),
    }
)


class RetryClass(StrEnum):
    RETRY_SAME_KEY = "retry_same_key"
    TERMINAL = "terminal"
    RETRY_READ = "retry_read"
    NETWORK = "network"


def classify(status: int, code: str) -> RetryClass:
    """Map an HTTP status + error code to its retry class."""
    if (status, code) in _RETRY_SAME_KEY:
        return RetryClass.RETRY_SAME_KEY
    # A 429 or 503 with an unexpected code (e.g. an edge proxy's own envelope) is still a
    # "come back later" signal and safe to retry with the same key.
    if status in (429, 503):
        return RetryClass.RETRY_SAME_KEY
    if 400 <= status < 500:
        return RetryClass.TERMINAL
    return RetryClass.RETRY_READ


class PlatformError(Exception):
    """Base class for everything the client raises."""

    retry_class: RetryClass


class ApiError(PlatformError):
    """The platform answered with an error envelope (or a non-JSON error body)."""

    def __init__(
        self,
        status: int,
        code: str,
        message: str,
        details: dict[str, Any] | None = None,
        *,
        retry_after_s: float | None = None,
        request_id: str | None = None,
        body: Any = None,
    ) -> None:
        super().__init__(f"{status} {code}: {message}")
        self.status = status
        self.code = code
        self.message = message
        self.details: dict[str, Any] = details or {}
        self.retry_after_s = retry_after_s
        self.request_id = request_id
        #: The full decoded body, for endpoints (batch) whose error statuses carry per-item results.
        self.body = body
        self.retry_class = classify(status, code)

    @property
    def retryable_write(self) -> bool:
        return self.retry_class is RetryClass.RETRY_SAME_KEY

    @property
    def outcome_unknown(self) -> bool:
        """True when a write may or may not have executed (reconcile or retry with the same key)."""
        return self.code == "ORDER_STATUS_UNKNOWN" or self.retry_class is RetryClass.RETRY_READ

    def __repr__(self) -> str:
        return f"ApiError(status={self.status}, code={self.code!r}, request_id={self.request_id!r})"


class NetworkError(PlatformError):
    """No HTTP response: connect failure, timeout, or a dropped connection."""

    retry_class = RetryClass.NETWORK

    def __init__(self, message: str, *, cause: BaseException | None = None) -> None:
        super().__init__(message)
        self.cause = cause
        self.outcome_unknown = True
