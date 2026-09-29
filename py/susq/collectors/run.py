"""One collection cycle: (re)discover, snapshot both venues, archive raw rows + reference mids.

Venues fail independently: a Kalshi outage still yields Polymarket quotes and vice versa. The
reference table (`data/reference/...`) is the only thing downstream code reads.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import polars as pl

from susq.collectors import kalshi, polymarket
from susq.collectors.http import PoliteClient
from susq.collectors.quotes import RefQuote
from susq.collectors.store import read_latest, write_snapshot

log = logging.getLogger(__name__)

DISCOVERY_TTL_S = 6 * 3600.0


@dataclass
class CollectResult:
    quotes: list[RefQuote] = field(default_factory=list)
    errors: dict[str, str] = field(default_factory=dict)
    files: list[Path] = field(default_factory=list)


class ReferenceCollector:
    """Holds venue clients and cached discovery results across cycles."""

    def __init__(
        self,
        data_dir: Path,
        *,
        kalshi_client: PoliteClient | None = None,
        gamma_client: PoliteClient | None = None,
        clob_client: PoliteClient | None = None,
        discovery_ttl_s: float = DISCOVERY_TTL_S,
        clock_ns: Callable[[], int] = time.time_ns,
    ) -> None:
        self.data_dir = data_dir
        self.kalshi = kalshi_client or PoliteClient(kalshi.BASE_URL, min_interval_s=0.25)
        self.gamma = gamma_client or PoliteClient(polymarket.GAMMA_URL, min_interval_s=0.2)
        self.clob = clob_client or PoliteClient(polymarket.CLOB_URL, min_interval_s=0.2)
        self._ttl_ns = int(discovery_ttl_s * 1e9)
        self._clock_ns = clock_ns
        self._kalshi_universe: list[kalshi.KalshiContract] = []
        self._poly_universe: list[polymarket.PolyContract] = []
        self._kalshi_at = self._poly_at = -(10**30)

    def close(self) -> None:
        for c in (self.kalshi, self.gamma, self.clob):
            c.close()

    def collect_once(self) -> CollectResult:
        result = CollectResult()
        now = self._clock_ns()
        for venue, fn in (("kalshi", self._collect_kalshi), ("polymarket", self._collect_polymarket)):
            try:
                quotes, raw_path = fn(now)
            except Exception as exc:  # one venue down must not stop the other
                log.exception("%s collection failed", venue)
                result.errors[venue] = f"{type(exc).__name__}: {exc}"
                continue
            result.quotes.extend(quotes)
            if raw_path:
                result.files.append(raw_path)
        ref = write_snapshot(self.data_dir / "reference", [q.as_row() for q in result.quotes], now)
        if ref:
            result.files.append(ref)
        return result

    def _collect_kalshi(self, now: int) -> tuple[list[RefQuote], Path | None]:
        if now - self._kalshi_at > self._ttl_ns or not self._kalshi_universe:
            self._kalshi_universe = kalshi.discover(self.kalshi)
            self._kalshi_at = now
            log.info("kalshi universe: %d contracts", len(self._kalshi_universe))
        by_ticker = {c.ticker: c for c in self._kalshi_universe}
        markets = kalshi.fetch_markets(self.kalshi, by_ticker)
        raw, quotes = [], []
        for m in markets:
            c = by_ticker.get(str(m.get("ticker")))
            if c is None:
                continue
            raw.append(kalshi.raw_row(m, c, now))
            if (q := kalshi.to_quote(m, c, now)) is not None:
                quotes.append(q)
        return quotes, write_snapshot(self.data_dir / "kalshi", raw, now)

    def _collect_polymarket(self, now: int) -> tuple[list[RefQuote], Path | None]:
        if now - self._poly_at > self._ttl_ns or not self._poly_universe:
            self._poly_universe = polymarket.discover(self.gamma)
            self._poly_at = now
            log.info("polymarket universe: %d contracts", len(self._poly_universe))
        try:
            books = polymarket.fetch_books(self.clob, (c.yes_token for c in self._poly_universe))
        except Exception:
            log.exception("polymarket CLOB books failed; falling back to Gamma top of book")
            books = {}
        raw, quotes = [], []
        for c in self._poly_universe:
            tob = books.get(c.yes_token)
            raw.append(polymarket.raw_row(c, tob, now))
            if (q := polymarket.to_quote(c, tob, now)) is not None:
                quotes.append(q)
        return quotes, write_snapshot(self.data_dir / "polymarket", raw, now)


def load_latest_quotes(data_dir: Path) -> list[RefQuote]:
    """The most recent reference snapshot, as RefQuote rows (empty if none)."""
    df = read_latest(data_dir / "reference")
    return quotes_from_frame(df)


def quotes_from_frame(df: pl.DataFrame) -> list[RefQuote]:
    if df.is_empty():
        return []
    rows: list[dict[str, Any]] = (
        df.with_columns(pl.col("ts").dt.epoch("ns").alias("ts_ns")).drop("ts").to_dicts()
    )
    fields = RefQuote.__dataclass_fields__
    return [RefQuote(**{k: r[k] for k in fields}) for r in rows]
