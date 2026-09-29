"""Read Cup market listings recorded by the bridge (`data/cup/markets/markets-*.json`).

The recorder stores raw `GET /tournaments/{slug}/markets` responses. We accept the shapes that
can reasonably appear on disk: a single page (`{"data": [...], "pagination": ...}`), a list of
pages, or a flat list of market rows. A row may carry its resolution tree under `nodes`
(the `GET /markets/{id}/nodes` response) when the recorder fetched it.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class CupExchange:
    exchange_id: str
    option: str | None
    latest_price: float | None


@dataclass(frozen=True)
class CupMarket:
    market_id: str
    title: str
    status: str
    categories: tuple[str, ...]
    exchanges: tuple[CupExchange, ...]
    nodes: dict[str, Any] | None = field(default=None, compare=False)


def _rows(doc: Any) -> list[dict[str, Any]]:
    if isinstance(doc, dict):
        return list(doc.get("data") or [])
    if isinstance(doc, list):
        out: list[dict[str, Any]] = []
        for item in doc:
            if isinstance(item, dict) and "data" in item and "id" not in item:
                out.extend(item.get("data") or [])
            elif isinstance(item, dict):
                out.append(item)
        return out
    raise ValueError("unrecognized Cup markets document")


def _category_names(raw: Any) -> tuple[str, ...]:
    names = []
    for c in raw or []:
        names.append(str(c.get("name") or c.get("slug") or "") if isinstance(c, dict) else str(c))
    return tuple(n for n in names if n)


def parse_markets(doc: Any) -> list[CupMarket]:
    """Normalize one recorded document into CupMarket rows (deduplicated by market id)."""
    seen: dict[str, CupMarket] = {}
    for row in _rows(doc):
        exchanges = tuple(
            CupExchange(
                exchange_id=str(x["id"]),
                option=x.get("option"),
                latest_price=float(x["latestPrice"]) if x.get("latestPrice") is not None else None,
            )
            for x in row.get("exchanges") or []
        )
        market = CupMarket(
            market_id=str(row["id"]),
            title=str(row.get("title") or ""),
            status=str(row.get("status") or ""),
            categories=_category_names(row.get("categories")),
            exchanges=exchanges,
            nodes=row.get("nodes"),
        )
        seen[market.market_id] = market
    return list(seen.values())


def latest_markets_file(cup_dir: Path) -> Path | None:
    """Newest `markets-*.json` under `data/cup/markets/` (names sort by timestamp)."""
    files = sorted((cup_dir / "markets").glob("markets-*.json"))
    return files[-1] if files else None


def load_latest(cup_dir: Path) -> list[CupMarket]:
    path = latest_markets_file(cup_dir)
    return parse_markets(json.loads(path.read_text())) if path else []


def node_text(nodes: dict[str, Any] | None) -> str:
    """Concatenated human-readable text of a node tree (titles, contract details, options)."""
    if not nodes:
        return ""
    parts: list[str] = []

    def walk(node: Any) -> None:
        if isinstance(node, dict):
            for key in ("title", "contractType"):
                if isinstance(node.get(key), str):
                    parts.append(node[key])
            details = node.get("contractDetails")
            if isinstance(details, dict):
                parts.extend(str(v) for v in details.values() if isinstance(v, (str, int, float)))
            for child in node.get("children") or []:
                walk(child)

    walk(nodes.get("root", nodes))
    return " | ".join(parts)


def resolution_source(nodes: dict[str, Any] | None) -> str:
    """Best-effort resolution source from contract details (the Info-tab prose is not in the API)."""
    if not nodes:
        return ""
    stack = [nodes.get("root", nodes)]
    while stack:
        node = stack.pop()
        if not isinstance(node, dict):
            continue
        details = node.get("contractDetails")
        if isinstance(details, dict):
            for key, value in details.items():
                if "source" in key.lower() and isinstance(value, str) and value:
                    return value
        stack.extend(node.get("children") or [])
    return ""
