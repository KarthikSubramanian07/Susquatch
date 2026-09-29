"""A deliberately polite read-only HTTP client for public market-data endpoints.

We are guests on Kalshi's and Polymarket's free public APIs, so every request goes through a
minimum inter-request gap, retries only on 429/5xx/network errors, and honours `Retry-After`.
Kalshi's Basic tier allows ~20 reads/s and Polymarket's `/books` 50/s; we default to 4/s and 5/s.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Callable
from typing import Any

import httpx

log = logging.getLogger(__name__)

USER_AGENT = "susquatch-research/0.1 (+read-only public data)"
_RETRY_STATUS = frozenset({429, 500, 502, 503, 504})


class PoliteClient:
    """Wraps `httpx.Client` with pacing and bounded retries. Not thread-safe; one per collector."""

    def __init__(
        self,
        base_url: str,
        *,
        min_interval_s: float = 0.25,
        max_retries: int = 4,
        timeout_s: float = 20.0,
        transport: httpx.BaseTransport | None = None,
        sleep: Callable[[float], None] = time.sleep,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._client = httpx.Client(
            base_url=base_url,
            timeout=timeout_s,
            headers={"User-Agent": USER_AGENT, "Accept": "application/json"},
            transport=transport,
        )
        self._min_interval = min_interval_s
        self._max_retries = max_retries
        self._sleep = sleep
        self._clock = clock
        self._last = -1e18
        self.requests = 0

    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> PoliteClient:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def get_json(self, path: str, params: dict[str, Any] | None = None) -> Any:
        return self._request("GET", path, params=params)

    def post_json(self, path: str, body: Any) -> Any:
        return self._request("POST", path, json=body)

    def _pace(self) -> None:
        wait = self._last + self._min_interval - self._clock()
        if wait > 0:
            self._sleep(wait)
        self._last = self._clock()

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        attempt = 0
        while True:
            self._pace()
            self.requests += 1
            try:
                resp = self._client.request(method, path, **kwargs)
            except httpx.TransportError as exc:
                if attempt >= self._max_retries:
                    raise
                delay = _backoff(attempt)
                log.warning("network error on %s %s (%s); retry in %.1fs", method, path, exc, delay)
            else:
                if resp.status_code < 400:
                    return resp.json()
                if resp.status_code not in _RETRY_STATUS or attempt >= self._max_retries:
                    resp.raise_for_status()
                delay = _retry_after(resp) or _backoff(attempt)
                log.warning("%s on %s %s; retry in %.1fs", resp.status_code, method, path, delay)
            self._sleep(delay)
            attempt += 1


def _backoff(attempt: int) -> float:
    return float(min(30.0, 2.0**attempt))


def _retry_after(resp: httpx.Response) -> float | None:
    value = resp.headers.get("Retry-After")
    if value is None:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        return None
