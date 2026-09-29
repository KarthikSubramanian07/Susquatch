"""Read/write `susq.v1.MarketMap` JSON files atomically."""

from __future__ import annotations

from pathlib import Path

from susq.contracts import from_json, ops_pb2, to_json
from susq.research_env import Paths, atomic_write_text


def read_market_map(path: Path) -> ops_pb2.MarketMap | None:
    return from_json(ops_pb2.MarketMap, path.read_text()) if path.exists() else None


def write_market_map(path: Path, mm: ops_pb2.MarketMap) -> None:
    atomic_write_text(path, to_json(mm))


def load_best_map(paths: Paths) -> ops_pb2.MarketMap | None:
    """The live (human-verified) map if present, else our latest draft."""
    return read_market_map(paths.market_map) or read_market_map(paths.market_map_draft)
