"""Hive-partitioned Parquet (zstd) snapshots: `<root>/date=YYYY-MM-DD/part-<utc>.parquet`.

One file per snapshot keeps writes append-only and crash-safe (temp file + rename). Nightly
compaction into daily files is the backup job's business, not the collector's.
"""

from __future__ import annotations

import os
from collections.abc import Sequence
from datetime import UTC, date, datetime
from pathlib import Path
from typing import Any

import polars as pl

TS_DTYPE = pl.Datetime("us", "UTC")


def _stamp(ts_ns: int) -> datetime:
    return datetime.fromtimestamp(ts_ns / 1e9, tz=UTC)


def write_snapshot(root: Path, rows: Sequence[dict[str, Any]], ts_ns: int) -> Path | None:
    """Write `rows` as one partition file; `ts_ns` becomes a `ts` timestamp[us, UTC] column."""
    if not rows:
        return None
    df = pl.DataFrame(list(rows), infer_schema_length=None)
    if "ts_ns" in df.columns:
        df = df.with_columns(pl.from_epoch(pl.col("ts_ns"), time_unit="ns").cast(TS_DTYPE).alias("ts")).drop(
            "ts_ns"
        )
    stamp = _stamp(ts_ns)
    out = root / f"date={stamp:%Y-%m-%d}" / f"part-{stamp:%Y%m%dT%H%M%S%fZ}.parquet"
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(f".{out.name}.tmp")
    df.write_parquet(tmp, compression="zstd")
    os.replace(tmp, out)
    return out


def partition_days(
    root: Path, *, since: date | None = None, until: date | None = None
) -> list[tuple[date, list[Path]]]:
    """`(day, files)` per `date=` partition under `root`, oldest first, optionally limited (inclusive)."""
    if not root.exists():
        return []
    days: list[tuple[date, list[Path]]] = []
    for part in sorted(root.glob("date=*")):
        day = date.fromisoformat(part.name.removeprefix("date="))
        if (since and day < since) or (until and day > until):
            continue
        days.append((day, sorted(part.glob("*.parquet"))))
    return days


def partition_files(root: Path, *, since: date | None = None, until: date | None = None) -> list[Path]:
    """Snapshot files under `root`, oldest first, optionally limited to a date range (inclusive)."""
    return [f for _, files in partition_days(root, since=since, until=until) for f in files]


def read_snapshots(
    root: Path,
    *,
    since: date | None = None,
    until: date | None = None,
    columns: Sequence[str] | None = None,
) -> pl.DataFrame:
    """Concatenate every snapshot in range (schema drift tolerated); empty frame if none.

    With `columns`, each file is projected *before* the concat: a projection applied after a
    `diagonal_relaxed` concat is not pushed down into the scans, so polars decoded every column of
    every file. Over the reference tree (1,500 files a day) that is what took the scheduler past its
    2 GB cap. Callers that need many days should still fold one `partition_days` entry at a time.
    """
    files = partition_files(root, since=since, until=until)
    if not files:
        return pl.DataFrame()
    if columns is None:
        scans = [pl.scan_parquet(f) for f in files]
    else:
        scans = []
        for f in files:
            keep = [c for c in columns if c in pl.read_parquet_schema(f)]
            scans.append(pl.scan_parquet(f).select(keep))
    return pl.concat(scans, how="diagonal_relaxed").collect()


def read_latest(root: Path) -> pl.DataFrame:
    """Only the most recent snapshot file."""
    files = partition_files(root)
    return pl.read_parquet(files[-1]) if files else pl.DataFrame()
