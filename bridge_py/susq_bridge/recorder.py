"""Market-data recorder for research: Cup prices, leaderboard snapshots, market lists (Parquet/JSON).

Layout (all Competition Data, gitignored):

* ``data/cup/prices/date=YYYY-MM-DD/part-<ts>.parquet`` - ts, exchange_id, market_id, latest_price,
  best_bid, best_ask, spread;
* ``data/cup/leaderboard/date=YYYY-MM-DD/part-<ts>.parquet`` - ts, rank, profile_id, username, pnl,
  trades_count, volume, win_rate, roi, total, my_rank;
* ``data/cup/markets/markets-<ts>.json`` - the raw market list plus each market's node tree.

Parquet files are zstd-compressed and written atomically (tmp + rename) so readers never see a
partial file.
"""

from __future__ import annotations

import os
from collections.abc import Callable, Iterable, Mapping
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

import orjson
import polars as pl
from susq.platform.models import Leaderboard, Market, MarketNodes, PriceSnapshot

if TYPE_CHECKING:
    from susq_bridge.gateway import Gateway
    from susq_bridge.market_state import MarketState

__all__ = ["LEADERBOARD_SCHEMA", "PRICES_SCHEMA", "Recorder"]

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
