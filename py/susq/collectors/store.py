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


def partition_files(root: Path, *, since: date | None = None, until: date | None = None) -> list[Path]:
    """Snapshot files under `root`, oldest first, optionally limited to a date range (inclusive)."""
    if not root.exists():
        return []
    files: list[Path] = []
    for part in sorted(root.glob("date=*")):
        day = date.fromisoformat(part.name.removeprefix("date="))
        if (since and day < since) or (until and day > until):
            continue
        files.extend(sorted(part.glob("*.parquet")))
    return files


def read_snapshots(root: Path, *, since: date | None = None, until: date | None = None) -> pl.DataFrame:
    """Concatenate every snapshot in range (schema drift tolerated); empty frame if none."""
    files = partition_files(root, since=since, until=until)
    if not files:
        return pl.DataFrame()
    return pl.concat([pl.read_parquet(f) for f in files], how="diagonal_relaxed")


def read_latest(root: Path) -> pl.DataFrame:
    """Only the most recent snapshot file."""
    files = partition_files(root)
    return pl.read_parquet(files[-1]) if files else pl.DataFrame()
