"""Market-data recorder for research: Cup prices, leaderboard snapshots, market lists (Parquet/JSON).

Layout (all Competition Data, gitignored):

* ``data/cup/prices/date=YYYY-MM-DD/part-<ts>.parquet`` - ts, exchange_id, market_id, latest_price,
  best_bid, best_ask, spread;
* ``data/cup/leaderboard/date=YYYY-MM-DD/part-<ts>.parquet`` - ts, rank, profile_id, username, pnl,
  trades_count, volume, win_rate, roi, total, my_rank;
* ``data/cup/markets/markets-<ts>.json`` - the raw market list plus each market's node tree;
* ``data/cup/tape/date=YYYY-MM-DD/part-<ts>.parquet`` - the public trade tape: ts (the trade's
  ``createdAt``), exchange_id, market_id, trade_id, side (YES/NO, the taker's space), price, size,
  volume (nullable), recorded_at. Partitioned by the trade's day, one file per recording pass and
  day, deduplicated by trade_id within a pass;
* ``data/cup/tape/state.json`` - per-exchange cursor state so a pass resumes where the last one
  stopped (the newest fully recorded trade, plus a mid-walk page cursor when a pass hit its request
  cap).

The tape is what the leaderboard cannot show: which side the field actually bought, at what price,
in what size, per market. It is a small read-only consumer of the shared request budget: each pass
walks every exchange's tape newest-first and stops at the first trade it already holds, so steady
state costs one read per exchange per pass, and ``max_requests`` caps a backlog.

Parquet files are zstd-compressed and written atomically (tmp + rename) so readers never see a
partial file.
"""

from __future__ import annotations

import logging
import os
from collections.abc import Callable, Iterable, Mapping
from dataclasses import asdict, dataclass
from datetime import UTC, date, datetime
from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING, Any

import orjson
import polars as pl
from susq.platform.models import Leaderboard, Market, MarketNodes, PriceSnapshot, Trade

if TYPE_CHECKING:
    from susq_bridge.gateway import Gateway
    from susq_bridge.market_state import MarketState

__all__ = ["LEADERBOARD_SCHEMA", "PRICES_SCHEMA", "TAPE_SCHEMA", "Recorder", "TapeCursor"]

log = logging.getLogger(__name__)

TS = pl.Datetime(time_unit="us", time_zone="UTC")
PRICES_SCHEMA: dict[str, pl.DataType] = {
    "ts": TS,
    "exchange_id": pl.String(),
    "market_id": pl.String(),
    "latest_price": pl.Float64(),
    "best_bid": pl.Float64(),
    "best_ask": pl.Float64(),
    "spread": pl.Float64(),
}
LEADERBOARD_SCHEMA: dict[str, pl.DataType] = {
    "ts": TS,
    "rank": pl.Int32(),
    "profile_id": pl.String(),
    "username": pl.String(),
    "pnl": pl.Float64(),
    "trades_count": pl.Int32(),
    "volume": pl.Float64(),
    "win_rate": pl.Float64(),
    "roi": pl.Float64(),
    "total": pl.Int32(),
    "my_rank": pl.Int32(),
}
TAPE_SCHEMA: dict[str, pl.DataType] = {
    "ts": TS,
    "exchange_id": pl.String(),
    "market_id": pl.String(),
    "trade_id": pl.String(),
    "side": pl.String(),
    "price": pl.Float64(),
    "size": pl.Int64(),
    "volume": pl.Float64(),
    "recorded_at": TS,
}
TAPE_PAGE_SIZE = 200  # the API's maximum; fewer pages per exchange is what keeps the tape cheap


@dataclass
class TapeCursor:
    """Where one exchange's tape walk stands.

    ``last_id``/``last_created_at`` is the newest trade fully recorded: the floor the next walk stops
    at. A walk that is cut short (request cap, error) keeps ``pending_cursor`` (the next page to ask
    for) and ``head_id``/``head_created_at`` (the newest trade it saw before the cut) so the next pass
    resumes the same walk and only promotes the head to the floor once it reaches the old floor.
    """

    last_id: str | None = None
    last_created_at: datetime | None = None
    pending_cursor: str | None = None
    head_id: str | None = None
    head_created_at: datetime | None = None

    def known(self, trade: Trade) -> bool:
        if self.last_id is not None and trade.id == self.last_id:
            return True
        return (
            self.last_created_at is not None
            and trade.created_at is not None
            and trade.created_at <= self.last_created_at
        )

    def see(self, trade: Trade) -> None:
        """Track the newest trade of the walk in progress (the floor once the walk completes)."""
        if trade.created_at is None:
            return
        if self.head_created_at is None or trade.created_at > self.head_created_at:
            self.head_id, self.head_created_at = trade.id, trade.created_at

    def complete(self) -> None:
        if self.head_created_at is not None:
            self.last_id, self.last_created_at = self.head_id, self.head_created_at
        self.pending_cursor = self.head_id = self.head_created_at = None

    def abandon(self) -> None:
        """Drop an unfinished walk (an expired cursor, say); the floor is untouched so nothing is lost."""
        self.pending_cursor = self.head_id = self.head_created_at = None

    def to_json(self) -> dict[str, Any]:
        return {k: (v.isoformat() if isinstance(v, datetime) else v) for k, v in asdict(self).items()}

    @classmethod
    def from_json(cls, raw: Mapping[str, Any]) -> TapeCursor:
        def dt(key: str) -> datetime | None:
            value = raw.get(key)
            if not value:
                return None
            parsed = datetime.fromisoformat(str(value))
            return parsed if parsed.tzinfo else parsed.replace(tzinfo=UTC)

        return cls(
            last_id=raw.get("last_id"),
            last_created_at=dt("last_created_at"),
            pending_cursor=raw.get("pending_cursor"),
            head_id=raw.get("head_id"),
            head_created_at=dt("head_created_at"),
        )


def _newest_first(trades: list[Trade]) -> bool:
    stamped = [t.created_at for t in trades if t.created_at is not None]
    return len(stamped) < 2 or stamped[0] >= stamped[-1]


def _stamp(ts: datetime) -> str:
    return ts.astimezone(UTC).strftime("%Y%m%dT%H%M%S%fZ")


def _atomic_parquet(frame: pl.DataFrame, path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.tmp")
    frame.write_parquet(tmp, compression="zstd")
    os.replace(tmp, path)
    return path


class Recorder:
    def __init__(self, data_dir: Path, *, clock: Callable[[], datetime] = lambda: datetime.now(UTC)) -> None:
        self.root = data_dir / "cup"
        self.clock = clock
        self._tape_busy = False

    @property
    def tape_state_path(self) -> Path:
        return self.root / "tape" / "state.json"

    def _partition(self, kind: str, ts: datetime) -> Path:
        return (
            self.root / kind / f"date={ts.astimezone(UTC).date().isoformat()}" / f"part-{_stamp(ts)}.parquet"
        )

    def write_prices(self, snapshots: Iterable[PriceSnapshot], market_of: Mapping[str, str]) -> Path | None:
        ts = self.clock()
        rows = [
            {
                "ts": ts,
                "exchange_id": s.exchange_id,
                "market_id": s.market_id or market_of.get(s.exchange_id, ""),
                "latest_price": s.latest_price,
                "best_bid": s.best_bid,
                "best_ask": s.best_ask,
                "spread": s.spread,
            }
            for s in snapshots
        ]
        if not rows:
            return None
        return _atomic_parquet(pl.DataFrame(rows, schema=PRICES_SCHEMA), self._partition("prices", ts))

    def write_leaderboard(self, pages: Iterable[Leaderboard]) -> Path | None:
        ts = self.clock()
        rows: list[dict[str, Any]] = []
        for page in pages:
            for e in page.leaderboard:
                rows.append(
                    {
                        "ts": ts,
                        "rank": e.rank,
                        "profile_id": e.profile_id,
                        "username": e.username or "",
                        "pnl": e.pnl,
                        "trades_count": e.trades_count,
                        "volume": e.volume,
                        "win_rate": e.win_rate,
                        "roi": e.roi,
                        "total": page.total,
                        "my_rank": page.my_rank,
                    }
                )
        if not rows:
            return None
        return _atomic_parquet(
            pl.DataFrame(rows, schema=LEADERBOARD_SCHEMA), self._partition("leaderboard", ts)
        )

    def write_markets(self, markets: Iterable[Market], nodes: Mapping[str, MarketNodes]) -> Path:
        ts = self.clock()
        payload = {
            "fetchedAt": ts.isoformat(),
            "markets": [m.model_dump(mode="json", by_alias=True) for m in markets],
            "nodes": {mid: n.model_dump(mode="json", by_alias=True) for mid, n in nodes.items()},
        }
        path = self.root / "markets" / f"markets-{_stamp(ts)}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(f".{path.name}.tmp")
        tmp.write_bytes(orjson.dumps(payload, option=orjson.OPT_INDENT_2))
        os.replace(tmp, path)
        return path

    # ── trade tape ──────────────────────────────────────────────────────────────────────────

    def write_tape(self, exchange_id: str, trades: Iterable[Trade], *, market_id: str = "") -> list[Path]:
        """Record one exchange's trades; returns the files written (one per trade day)."""
        return self._write_tape_rows(self._tape_rows(exchange_id, market_id, trades, self.clock()))

    @staticmethod
    def _tape_rows(
        exchange_id: str, market_id: str, trades: Iterable[Trade], recorded_at: datetime
    ) -> list[dict[str, Any]]:
        return [
            {
                "ts": t.created_at or recorded_at,
                "exchange_id": exchange_id,
                "market_id": market_id,
                "trade_id": t.id,
                "side": (t.side or "").upper() or None,
                "price": t.price,
                "size": t.size,
                "volume": t.volume,
                "recorded_at": recorded_at,
            }
            for t in trades
        ]

    def _write_tape_rows(self, rows: list[dict[str, Any]]) -> list[Path]:
        """One zstd Parquet per trade day touched, trades deduplicated by trade_id."""
        if not rows:
            return []
        frame = (
            pl.DataFrame(rows, schema=TAPE_SCHEMA)
            .unique(subset="trade_id", keep="first", maintain_order=True)
            .sort("ts", "trade_id")
        )
        recorded_at: datetime = frame["recorded_at"][0]
        written: list[Path] = []
        by_day = frame.with_columns(pl.col("ts").dt.date().alias("day"))
        for (day,), part in by_day.group_by("day", maintain_order=True):
            assert isinstance(day, date)
            path = self.root / "tape" / f"date={day.isoformat()}" / f"part-{_stamp(recorded_at)}.parquet"
            written.append(_atomic_parquet(part.drop("day"), path))
        return written

    def load_tape_state(self) -> dict[str, TapeCursor]:
        path = self.tape_state_path
        if not path.exists():
            return {}
        try:
            raw = orjson.loads(path.read_bytes())
            return {str(ex): TapeCursor.from_json(c) for ex, c in raw.get("exchanges", {}).items()}
        except (orjson.JSONDecodeError, ValueError, TypeError, AttributeError):
            log.exception("tape cursor state unreadable; walking every tape from the top")
            return {}

    def save_tape_state(self, state: Mapping[str, TapeCursor]) -> Path:
        path = self.tape_state_path
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "savedAt": self.clock().isoformat(),
            "exchanges": {ex: c.to_json() for ex, c in sorted(state.items())},
        }
        tmp = path.with_name(f".{path.name}.tmp")
        tmp.write_bytes(orjson.dumps(payload, option=orjson.OPT_INDENT_2))
        os.replace(tmp, path)
        return path

    # ── async helpers used by the bridge loops ──────────────────────────────────────────────

    async def record_prices(self, market: MarketState) -> Path | None:
        prices = await market.refresh_prices()
        return self.write_prices(prices.values(), market.exchange_market)

    async def record_leaderboard(self, gateway: Gateway, *, max_pages: int = 50) -> Path | None:
        slug = gateway.config.platform.tournament_slug
        client = gateway.client
        pages: list[Leaderboard] = []
        offset = 0
        for _ in range(max_pages):
            page = await gateway.read(
                "leaderboard", lambda o=offset: client.get_leaderboard(slug, limit=100, offset=o)
            )
            pages.append(page)
            offset += len(page.leaderboard)
            if not page.leaderboard or offset >= page.total:
                break
        return self.write_leaderboard(pages)

    async def snapshot_markets(self, gateway: Gateway, market: MarketState) -> Path:
        markets = await market.load_markets()
        client = gateway.client
        tid = market.tournament_id
        nodes: dict[str, MarketNodes] = {}
        for m in markets:
            nodes[m.id] = await gateway.read("nodes", lambda mid=m.id: client.get_market_nodes(mid, tid))
        return self.write_markets(markets, nodes)

    async def record_tape(
        self, gateway: Gateway, market: MarketState, *, max_requests: int = 600
    ) -> list[Path]:
        """One recording pass over every exchange's public trade tape; returns the files written.

        Each exchange is walked newest-first through ``GET /exchanges/{id}/trades`` (label ``tape`` in
        the limiter, never a priority read) until a trade already on disk shows up, so a steady-state
        pass costs one read per exchange. ``max_requests`` bounds the pass; a walk cut short keeps its
        page cursor in ``state.json`` and the next pass picks it up, so the first pass (history since
        Oct 1) just spreads over however many passes it needs. Only one pass runs at a time; a second
        call while one is in flight is a no-op. Errors on one exchange are logged and the rest of the
        pass continues; whatever was collected is written and the cursor state is saved afterwards.
        """
        if self._tape_busy:
            log.warning("tape pass skipped: previous pass still in flight")
            return []
        self._tape_busy = True
        try:
            return await self._tape_pass(gateway, market, max_requests)
        finally:
            self._tape_busy = False

    async def _tape_pass(self, gateway: Gateway, market: MarketState, max_requests: int) -> list[Path]:
        state = self.load_tape_state()
        budget = max(0, int(max_requests))
        recorded_at = self.clock()
        rows: list[dict[str, Any]] = []
        exchanges = sorted(market.exchange_market)
        # Unfinished walks go first so a backlog converges instead of being restarted every pass.
        exchanges.sort(key=lambda ex: state.get(ex, TapeCursor()).pending_cursor is None)
        capped = False
        for ex in exchanges:
            if budget <= 0:
                capped = True
                break
            cursor = state.setdefault(ex, TapeCursor())
            try:
                trades, budget = await self._walk_tape(gateway, ex, market.tournament_id, cursor, budget)
            except Exception:
                log.exception("tape walk failed for exchange %s; restarting that walk next pass", ex)
                cursor.abandon()
                continue
            rows.extend(self._tape_rows(ex, market.exchange_market.get(ex, ""), trades, recorded_at))
        try:
            written = self._write_tape_rows(rows)
        finally:
            # State is saved after the parquet write: a crash in between repeats trades, never loses them.
            self.save_tape_state(state)
        pending = sum(1 for c in state.values() if c.pending_cursor is not None)
        if capped or pending:
            log.info(
                "tape pass wrote %d trades, used the %d-request cap; %d exchange(s) resume next pass",
                len(rows),
                max_requests,
                pending,
            )
        return written

    async def _walk_tape(
        self, gateway: Gateway, exchange_id: str, tournament_id: str, cursor: TapeCursor, budget: int
    ) -> tuple[list[Trade], int]:
        """Page one exchange's tape from the pending cursor (or the top) down to the floor.

        Returns the new trades (newest first) and the remaining request budget. The floor is passed as
        ``from`` so a server that honours it bounds the walk itself; the known-trade check does the
        same for one that ignores it.
        """
        client = gateway.client
        page_cursor = cursor.pending_cursor
        floor = cursor.last_created_at.isoformat() if cursor.last_created_at else None
        new: list[Trade] = []
        while budget > 0:
            budget -= 1
            fetch = partial(
                client.get_trades,
                exchange_id,
                tournament_id,
                limit=TAPE_PAGE_SIZE,
                cursor=page_cursor,
                from_=floor,
            )
            page = await gateway.read("tape", fetch)
            reached_floor = False
            for t in page.data:
                if cursor.known(t):
                    reached_floor = True
                    continue
                cursor.see(t)
                new.append(t)
            next_cursor = page.pagination.next_cursor if page.pagination.has_more else None
            # A known trade ends the walk only when pages run newest-first (older pages hold nothing
            # new). A server that pages oldest-first from ``from`` is walked to its last page instead.
            if reached_floor and not _newest_first(page.data):
                reached_floor = False
            if reached_floor or not next_cursor or not page.data:
                cursor.complete()
                return new, budget
            page_cursor = next_cursor
            cursor.pending_cursor = next_cursor
        return new, budget
