"""Polymarket: Gamma API for discovery, CLOB `POST /books` for quotes (both public, no auth).

Gamma `GET /events?tag_slug=midterms` returns every midterm event with its markets; we keep the
race-winner and chamber-control events and, inside them, the active party markets (Polymarket
pads multi-outcome events with inactive "Person A"/"Option B" placeholders, which we drop).
Gamma's own `bestBid/bestAsk` are the fallback when a CLOB book is unavailable.

Verified live 2026-09-28: `POST https://clob.polymarket.com/books` accepts a JSON list of
`{"token_id": ...}` (500 per call worked; we send 100) and returns books whose bids ascend and
asks descend, so the best levels are max(bids) and min(asks).
"""

from __future__ import annotations

import json
import logging
import re
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from typing import Any

from susq.collectors.http import PoliteClient
from susq.collectors.quotes import POLYMARKET, RefQuote, blend_mid, to_float
from susq.mapping.races import DEM, IND, REP, RaceKey, parse_party, parse_race

log = logging.getLogger(__name__)

GAMMA_URL = "https://gamma-api.polymarket.com"
CLOB_URL = "https://clob.polymarket.com"
TAG_SLUG = "midterms"
_EVENT_SLUG = re.compile(
    r"^which-party-will-win-the-(?:senate|house)-in-2026$"
    r"|-senate-election-winner(?:-\d+)?$"
    r"|-governor-(?:election-)?winner(?:-2026)?$"
    r"|-house-election-winner(?:-\d+)?$"
    r"|^which-party-will-win-the-house-race-for-the-[a-z]{2}-\d{1,2}-seat$"
)


@dataclass(frozen=True)
class PolyContract:
    """A discovered Polymarket market mapped to a race and party."""

    market_id: str
    event_slug: str
    question: str
    yes_token: str
    no_token: str
    race: RaceKey
    party: str
    gamma_bid: float | None = None
    gamma_ask: float | None = None
    gamma_last: float | None = None
    volume: float = 0.0


def is_race_event(slug: str) -> bool:
    """True for race-winner and chamber-control event slugs of this cycle."""
    return bool(_EVENT_SLUG.search(slug)) and "2028" not in slug and "individual" not in slug


def classify(event: dict[str, Any], market: dict[str, Any]) -> PolyContract | None:
    """Map one Gamma market to (race, party), or None for placeholders and non-winner markets."""
    if not market.get("active") or market.get("closed") or not market.get("clobTokenIds"):
        return None
    question = str(market.get("question") or "")
    race = parse_race(question)
    party = parse_party(question)
    if race is None or party not in {DEM, REP, IND}:
        return None
    try:
        tokens = json.loads(market["clobTokenIds"])
        outcomes = json.loads(market.get("outcomes") or '["Yes","No"]')
    except (TypeError, ValueError):
        return None
    if len(tokens) != 2 or [str(o).lower() for o in outcomes] != ["yes", "no"]:
        return None
    return PolyContract(
        market_id=str(market.get("id", "")),
        event_slug=str(event.get("slug", "")),
        question=question,
        yes_token=str(tokens[0]),
        no_token=str(tokens[1]),
        race=race,
        party=party,
        gamma_bid=to_float(market.get("bestBid")),
        gamma_ask=to_float(market.get("bestAsk")),
        gamma_last=to_float(market.get("lastTradePrice")),
        volume=to_float(market.get("volumeNum")) or 0.0,
    )


def iter_events(
    client: PoliteClient, *, page_size: int = 100, max_pages: int = 60
) -> Iterator[dict[str, Any]]:
    """Every open event under the midterms tag (offset pagination)."""
    for page_no in range(max_pages):
        params = {
            "tag_slug": TAG_SLUG,
            "closed": "false",
            "active": "true",
            "limit": page_size,
            "offset": page_no * page_size,
        }
        page = client.get_json("/events", params)
        if not page:
            return
        yield from page
        if len(page) < page_size:
            return
    log.warning("polymarket discovery stopped at max_pages=%d", max_pages)


def discover(client: PoliteClient) -> list[PolyContract]:
    """All 2026 party-winner and chamber-control contracts currently open on Polymarket."""
    out: list[PolyContract] = []
    for event in iter_events(client):
        if not is_race_event(str(event.get("slug", ""))):
            continue
        out.extend(c for m in event.get("markets") or [] if (c := classify(event, m)) is not None)
    return out


@dataclass(frozen=True)
class TopOfBook:
    bid: float | None
    ask: float | None
    bid_size: float | None
    ask_size: float | None
    last: float | None


def _levels(raw: Any) -> list[tuple[float, float | None]]:
    """(price, size) pairs with a parseable price."""
    out: list[tuple[float, float | None]] = []
    for level in raw or []:
        if (price := to_float(level.get("price"))) is not None:
            out.append((price, to_float(level.get("size"))))
    return out


def top_of_book(book: dict[str, Any]) -> TopOfBook:
    """Best levels from a CLOB book (order of levels is not trusted)."""
    bids = _levels(book.get("bids"))
    asks = _levels(book.get("asks"))
    best_bid: tuple[float | None, float | None] = max(bids, key=lambda x: x[0]) if bids else (None, None)
    best_ask: tuple[float | None, float | None] = min(asks, key=lambda x: x[0]) if asks else (None, None)
    return TopOfBook(
        best_bid[0], best_ask[0], best_bid[1], best_ask[1], to_float(book.get("last_trade_price"))
    )


def fetch_books(client: PoliteClient, tokens: Iterable[str], *, batch: int = 100) -> dict[str, TopOfBook]:
    """Top of book per YES token via `POST /books`."""
    tokens = list(tokens)
    out: dict[str, TopOfBook] = {}
    for i in range(0, len(tokens), batch):
        books = client.post_json("/books", [{"token_id": t} for t in tokens[i : i + batch]])
        for book in books or []:
            out[str(book.get("asset_id"))] = top_of_book(book)
    return out


def raw_row(contract: PolyContract, tob: TopOfBook | None, ts_ns: int) -> dict[str, Any]:
    """The archived raw row (CLOB when available, else Gamma's cached top of book)."""
    source = "clob" if tob is not None else "gamma"
    return {
        "ts_ns": ts_ns,
        "event_slug": contract.event_slug,
        "market_id": contract.market_id,
        "question": contract.question,
        "token_id": contract.yes_token,
        "best_bid": tob.bid if tob else contract.gamma_bid,
        "best_ask": tob.ask if tob else contract.gamma_ask,
        "bid_size": tob.bid_size if tob else None,
        "ask_size": tob.ask_size if tob else None,
        "last": tob.last if tob else contract.gamma_last,
        "volume": contract.volume,
        "source": source,
        "race_id": contract.race.race_id,
        "party": contract.party,
    }


def to_quote(contract: PolyContract, tob: TopOfBook | None, ts_ns: int) -> RefQuote | None:
    row = raw_row(contract, tob, ts_ns)
    mid = blend_mid(row["best_bid"], row["best_ask"], row["last"])
    if mid is None:
        return None
    return RefQuote(
        venue=POLYMARKET,
        race_id=contract.race.race_id,
        kind=contract.race.kind,
        party=contract.party,
        venue_market_id=contract.yes_token,
        title=contract.question,
        bid=row["best_bid"],
        ask=row["best_ask"],
        last=row["last"],
        mid=mid[0],
        spread=mid[1],
        volume=contract.volume,
        ts_ns=ts_ns,
    )
