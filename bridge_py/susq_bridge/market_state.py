"""Everything the strategies know about the market: tournament, markets, books, rules and fair values.

Fair values come from ``state/fair_live.json``: a ``susq.v1.TargetFile`` JSON carrying only ``header``
and ``fair``, rewritten by research every ~15 s. Two gates stand between a fair value and a trade:

* **staleness** - the file's ``header.produced_at_ns`` must be younger than ``fair.stale_after_s``;
* **mapping error** - if ``|Cup mid - fair|`` exceeds ``fair.mapping_error_pts`` the Cup and the
  reference market are probably mis-mapped (an inverted D/R, the wrong race). The market is blocked
  and flagged instead of traded.

Market rules come from ``state/market_map.json`` (``susq.v1.MarketMap``); strategies trade only rows
with ``verified: true``.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING

from susq.contracts import from_json, ops_pb2, targets_pb2
from susq.platform.models import Market, OrderBook, PriceSnapshot, Tournament
from susq.platform.prices import to_pips

from susq_bridge.config import BridgeConfig

if TYPE_CHECKING:
    from susq_bridge.gateway import Gateway

__all__ = ["FairCheck", "MarketState"]


@dataclass(frozen=True, slots=True)
class FairCheck:
    ok: bool
    reason: str  # "ok" | "no_fair" | "stale" | "unverified" | "no_cup_price" | "mapping_error"
    fair_pips: int | None = None
    age_s: float | None = None


class MarketState:
    def __init__(
        self,
        gateway: Gateway,
        config: BridgeConfig,
        *,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
    ) -> None:
        self.gateway = gateway
        self.config = config
        self.clock = clock
        self.tournament: Tournament | None = None
        self.markets: dict[str, Market] = {}
        self.exchange_market: dict[str, str] = {}
        self.rules: dict[str, ops_pb2.MarketRule] = {}
        self.prices: dict[str, PriceSnapshot] = {}
        self.prices_at: datetime | None = None
        self.fair: dict[str, targets_pb2.FairValue] = {}
        self.fair_produced_at: datetime | None = None
        self.flagged: dict[str, str] = {}
        self._fair_mtime: float | None = None
        self._books: dict[str, tuple[datetime, OrderBook]] = {}

    @property
    def tournament_id(self) -> str:
        if self.tournament is None:
            raise RuntimeError("tournament not loaded")
        return self.tournament.id

    # ── platform reads ──────────────────────────────────────────────────────────────────────

    async def load_tournament(self) -> Tournament:
        slug = self.config.platform.tournament_slug
        client = self.gateway.client
        self.tournament = await self.gateway.read("tournament", lambda: client.get_tournament(slug))
        return self.tournament

    async def load_markets(self) -> list[Market]:
        slug = self.config.platform.tournament_slug
        client = self.gateway.client
        markets: list[Market] = []
        cursor: str | None = None
        while True:
            page = await self.gateway.read(
                "markets", lambda c=cursor: client.list_tournament_markets(slug, status="open", cursor=c)
            )
            markets.extend(page.data)
            if not page.pagination.has_more or not page.pagination.next_cursor:
                break
            cursor = page.pagination.next_cursor
        self.markets = {m.id: m for m in markets}
        self.exchange_market = {ex.id: m.id for m in markets for ex in m.exchanges}
        return markets

    async def refresh_prices(self) -> dict[str, PriceSnapshot]:
        ids = sorted(self.exchange_market)
        client = self.gateway.client
        tid = self.tournament_id
        bulk = await self.gateway.read("prices", lambda: client.get_prices(ids, tid))
        self.prices = {p.exchange_id: p for p in bulk.data}
        self.prices_at = self.clock()
        return self.prices

    async def book(self, exchange_id: str, *, depth: int = 50, max_age_s: float = 0.5) -> OrderBook:
        cached = self._books.get(exchange_id)
        now = self.clock()
        if cached is not None and (now - cached[0]).total_seconds() <= max_age_s:
            return cached[1]
        client = self.gateway.client
        tid = self.tournament_id
        book = await self.gateway.read(
            "orderbook", lambda: client.get_orderbook(exchange_id, tid, depth=depth)
        )
        self._books[exchange_id] = (now, book)
        return book

    def invalidate_book(self, exchange_id: str | None = None) -> None:
        if exchange_id is None:
            self._books.clear()
        else:
            self._books.pop(exchange_id, None)

    # ── files ───────────────────────────────────────────────────────────────────────────────

    def load_market_map(self, path: Path | None = None) -> int:
        path = path or self.config.market_map_path
        if not path.exists():
            self.rules = {}
            return 0
        mm = from_json(ops_pb2.MarketMap, path.read_text())
        self.rules = {r.exchange_id: r for r in mm.markets if r.exchange_id}
        return len(self.rules)

    def load_fair(self, path: Path | None = None) -> bool:
        """Reload the fair-value file if it changed. Returns True when new values were loaded."""
        path = path or self.config.fair_path
        try:
            mtime = path.stat().st_mtime
        except FileNotFoundError:
            return False
        if mtime == self._fair_mtime:
            return False
        try:
            tf = from_json(targets_pb2.TargetFile, path.read_text())
        except Exception:  # a half-written file: keep the last good values, retry next tick
            return False
        self._fair_mtime = mtime
        self.fair = {fv.exchange_id: fv for fv in tf.fair if fv.exchange_id}
        produced = tf.header.produced_at_ns
        self.fair_produced_at = datetime.fromtimestamp(produced / 1e9, UTC) if produced else None
        return True

    # ── derived views ───────────────────────────────────────────────────────────────────────

    def mid(self, exchange_id: str) -> int | None:
        snap = self.prices.get(exchange_id)
        if snap is None:
            return None
        if snap.best_bid is not None and snap.best_ask is not None:
            return (to_pips(snap.best_bid) + to_pips(snap.best_ask)) // 2
        if snap.latest_price is not None:
            return to_pips(snap.latest_price)
        return None

    def marks(self) -> dict[str, int]:
        """Valuation marks: last tournament trade, falling back to the book mid."""
        out: dict[str, int] = {}
        for ex, snap in self.prices.items():
            if snap.latest_price is not None:
                out[ex] = to_pips(snap.latest_price)
            elif (m := self.mid(ex)) is not None:
                out[ex] = m
        return out

    def verified_rule(self, exchange_id: str) -> ops_pb2.MarketRule | None:
        rule = self.rules.get(exchange_id)
        return rule if rule is not None and rule.verified else None

    def fair_age_s(self) -> float | None:
        if self.fair_produced_at is None:
            return None
        return (self.clock() - self.fair_produced_at).total_seconds()

    def fair_check(self, exchange_id: str) -> FairCheck:
        """Is there a fresh, sane fair value for a verified market?"""
        if self.verified_rule(exchange_id) is None:
            return FairCheck(False, "unverified")
        fv = self.fair.get(exchange_id)
        if fv is None:
            return FairCheck(False, "no_fair")
        fair_pips = to_pips(fv.p_yes)
        age = self.fair_age_s()
        if age is None or age > self.config.fair.stale_after_s:
            return FairCheck(False, "stale", fair_pips, age)
        mid = self.mid(exchange_id)
        if mid is None:
            return FairCheck(False, "no_cup_price", fair_pips, age)
        if abs(mid - fair_pips) > self.config.fair.mapping_error_pts * 100:
            self.flagged[exchange_id] = f"mapping_error: cup {mid} vs fair {fair_pips}"
            return FairCheck(False, "mapping_error", fair_pips, age)
        self.flagged.pop(exchange_id, None)
        return FairCheck(True, "ok", fair_pips, age)
