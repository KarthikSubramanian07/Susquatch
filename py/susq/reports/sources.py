"""Load the day's operational records: journal entries, order intents, status snapshot, fills.

Formats (docs/RUNTIME.md): `journal/YYYY-MM-DD/<producer>.journal.jsonl` (one `JournalEntry` per
line), `state/<producer>/intents.jsonl` (`OrderIntent`, last line per intent_id wins),
`state/status.json` (`StatusSnapshot`). Fills are reconstructed from `POST /orders*` responses:
`quantityTraded` and the YES-denominated `fillPrice`, tied to their request via `correlation_id`
and to a strategy via the idempotency key.
"""

from __future__ import annotations

import json
import logging
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Any

from google.protobuf.json_format import ParseError

from susq.contracts import from_json, ops_pb2

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class Fill:
    ts_ns: int
    strategy: str
    exchange_id: str
    side: str  # yes | no
    action: str  # buy | sell
    quantity: float
    yes_price: float
    idempotency_key: str = ""

    @property
    def yes_exposure(self) -> float:
        """+q for YES-long fills (buy yes / sell no), −q otherwise."""
        long_yes = (self.side == "yes") == (self.action == "buy")
        return self.quantity if long_yes else -self.quantity


def _jsonl(path: Path) -> Iterator[str]:
    with path.open() as fh:
        for line in fh:
            if line.strip():
                yield line


def load_journal(journal_dir: Path, day: date) -> list[ops_pb2.JournalEntry]:
    out: list[ops_pb2.JournalEntry] = []
    for path in sorted((journal_dir / day.isoformat()).glob("*.journal.jsonl")):
        for line in _jsonl(path):
            try:
                out.append(from_json(ops_pb2.JournalEntry, line))
            except ParseError:
                log.warning("skipping malformed journal line in %s", path)
    out.sort(key=lambda e: e.ts_ns)
    return out


def load_intents(state_dir: Path) -> dict[str, ops_pb2.OrderIntent]:
    """Latest record per intent_id across every producer's intents.jsonl."""
    latest: dict[str, ops_pb2.OrderIntent] = {}
    for path in sorted(state_dir.glob("*/intents.jsonl")):
        for line in _jsonl(path):
            try:
                intent = from_json(ops_pb2.OrderIntent, line)
            except ParseError:
                log.warning("skipping malformed intent line in %s", path)
                continue
            latest[intent.intent_id] = intent
    return latest


def load_status(path: Path) -> ops_pb2.StatusSnapshot | None:
    return from_json(ops_pb2.StatusSnapshot, path.read_text()) if path.exists() else None


def _body(entry: ops_pb2.JournalEntry) -> Any:
    try:
        return json.loads(entry.body_json) if entry.body_json else None
    except json.JSONDecodeError:
        return None


def _order_items(body: Any) -> Iterable[dict[str, Any]]:
    """Order results in a single, batch (207) or multi-leg response."""
    if isinstance(body, dict):
        if "orderId" in body:
            yield body
        for key in ("results", "orders", "legs"):
            for item in body.get(key) or []:
                if isinstance(item, dict):
                    yield item.get("order", item) if "orderId" not in item else item


def _request_items(body: Any) -> list[dict[str, Any]]:
    if isinstance(body, dict):
        for key in ("orders", "legs"):
            if isinstance(body.get(key), list):
                return [x for x in body[key] if isinstance(x, dict)]
        return [body]
    return []


def extract_fills(
    entries: Iterable[ops_pb2.JournalEntry], intents: dict[str, ops_pb2.OrderIntent]
) -> list[Fill]:
    """Fills from order responses, attributed to strategies (unknown keys -> "unattributed")."""
    strategy_by_key = {i.idempotency_key: i.strategy for i in intents.values()}
    requests: dict[str, ops_pb2.JournalEntry] = {}
    fills: list[Fill] = []
    for e in entries:
        if e.method != "POST" or not e.path.startswith("/orders") or "cancel" in e.path:
            continue
        if e.direction == "request":
            requests[e.correlation_id] = e
            continue
        if e.direction != "response" or not (200 <= e.status < 300):
            continue
        req = requests.get(e.correlation_id)
        req_items = _request_items(_body(req)) if req else []
        for i, item in enumerate(_order_items(_body(e))):
            qty = float(item.get("quantityTraded") or 0)
            price = item.get("fillPrice")
            if qty <= 0 or price is None:
                continue
            req_item = req_items[i] if i < len(req_items) else {}
            key = e.idempotency_key or str(req_item.get("idempotencyKey", ""))
            fills.append(
                Fill(
                    ts_ns=e.ts_ns,
                    strategy=strategy_by_key.get(key, "unattributed"),
                    exchange_id=str(item.get("exchangeId") or req_item.get("exchangeId") or ""),
                    side=str(item.get("side") or req_item.get("side") or "yes").lower(),
                    action=str(item.get("action") or req_item.get("action") or "buy").lower(),
                    quantity=qty,
                    yes_price=float(price),
                    idempotency_key=key,
                )
            )
    return fills
