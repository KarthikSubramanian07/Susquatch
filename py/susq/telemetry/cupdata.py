"""Readers for Competition Data recorded by the bridge under `data/cup/` (never leaves the box).

Schemas (written by the bridge recorder):

* prices:      ts timestamp[us,UTC], exchange_id str, market_id str, latest_price f64?,
               best_bid f64?, best_ask f64?, spread f64?
* leaderboard: ts timestamp[us,UTC], rank i32, profile_id str, username str, pnl f64,
               trades_count i32, volume f64, win_rate f64?, roi f64, total i32, my_rank i32?

The directory layout under each root is the recorder's choice, so we read `**/*.parquet`.
"""

from __future__ import annotations

from collections.abc import Sequence
from datetime import date, datetime
from pathlib import Path

import polars as pl

from susq.collectors.quotes import blend_mid

PRICE_COLUMNS = ("ts", "exchange_id", "market_id", "latest_price", "best_bid", "best_ask", "spread")
LEADERBOARD_COLUMNS = (
    "ts", "rank", "profile_id", "username", "pnl", "trades_count", "volume", "win_rate", "roi", "total",
    "my_rank",
)  # fmt: skip


def _partition_day(part: Path) -> date | None:
    try:
        return date.fromisoformat(part.name.removeprefix("date="))
    except ValueError:
        return None


def read_tree(
    root: Path,
    *,
    since: datetime | None = None,
    newest_files: int | None = None,
    columns: Sequence[str] | None = None,
) -> pl.DataFrame:
    """Parquet files under `root` concatenated (empty frame if none), optionally ts >= since.

    The tree is partitioned as `date=YYYY-MM-DD/*.parquet`; with `since` only partitions on or after that day
    are opened at all, so the cost stays proportional to the lookback and not to the age of the data set.
    `newest_files` limits the read to the most recently modified files (cheap "latest" lookups); `columns`
    projects before reading, which is what keeps a 14-day scan of per-minute snapshots in seconds.
    """
    if not root.exists():
        return pl.DataFrame()
    files: list[Path] = sorted(root.glob("*.parquet"))  # unpartitioned files (tests, ad-hoc dumps)
    for part in sorted(p for p in root.iterdir() if p.is_dir()):
        day = _partition_day(part)
        if since is not None and day is not None and day < since.date():
            continue
        files.extend(sorted(part.rglob("*.parquet")))
    if newest_files is not None:
        files = sorted(files, key=lambda f: f.stat().st_mtime)[-newest_files:]
    if not files:
        return pl.DataFrame()
    lazy = pl.concat([pl.scan_parquet(f) for f in files], how="diagonal_relaxed")
    schema = lazy.collect_schema()
    if since is not None and "ts" in schema:
        lazy = lazy.filter(pl.col("ts") >= since)  # before the projection so `columns` need not keep ts
    if columns is not None:
        lazy = lazy.select([c for c in columns if c in schema])
    return lazy.collect()


def read_prices(
    cup_dir: Path, *, since: datetime | None = None, newest_files: int | None = None
) -> pl.DataFrame:
    df = read_tree(cup_dir / "prices", since=since, newest_files=newest_files)
    if df.is_empty():
        return df
    return df.with_columns(pl.col("exchange_id").cast(pl.Utf8), pl.col("market_id").cast(pl.Utf8))


def read_leaderboard(cup_dir: Path, *, since: datetime | None = None) -> pl.DataFrame:
    return read_tree(cup_dir / "leaderboard", since=since)


def with_cup_mid(prices: pl.DataFrame) -> pl.DataFrame:
    """Add `cup_mid` (two-sided mid, else a consistent last trade, else null)."""
    if prices.is_empty():
        return prices.with_columns(pl.lit(None, dtype=pl.Float64).alias("cup_mid"))
    mids = [
        (m[0] if (m := blend_mid(b, a, last)) else None)
        for b, a, last in zip(
            prices["best_bid"].to_list(),
            prices["best_ask"].to_list(),
            prices["latest_price"].to_list(),
            strict=True,
        )
    ]
    return prices.with_columns(pl.Series("cup_mid", mids, dtype=pl.Float64))


def latest_cup_mids(cup_dir: Path, *, newest_files: int = 5) -> dict[str, float]:
    """exchange_id -> most recent Cup mid (used for the mapping-error check)."""
    df = read_prices(cup_dir, newest_files=newest_files)
    if df.is_empty():
        return {}
    last = with_cup_mid(df.sort("ts").group_by("exchange_id").last())
    return {
        e: m
        for e, m in zip(last["exchange_id"].to_list(), last["cup_mid"].to_list(), strict=True)
        if m is not None
    }
