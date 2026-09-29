"""Kalshi public trade API v2: discover 2026 midterm winner markets and snapshot their quotes.

Discovery pages `GET /events?status=open&with_nested_markets=true` (≈60 pages, run a few times a
day) and keeps events whose ticker looks like a race-winner series for this cycle:
`SENATE{ST}[S]-26`, `GOVPARTY{ST}-26`, `HOUSE{ST}{N}-26`, `KXHOUSERACE-{ST}{NN}-26`,
`CONTROLH-2026`, `CONTROLS-2026`, ... Snapshots then use `GET /markets?tickers=` in batches of 100.

The race is parsed from the human title, not the ticker, because tickers are not always right
(observed 2026-09-28: `SENATELA-26` is titled "Kentucky Senate winner?"). A disagreement between
the ticker's state and the title's state is kept in the universe as a warning.
"""

from __future__ import annotations

import logging
import re
from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from typing import Any

from susq.collectors.http import PoliteClient
from susq.collectors.quotes import KALSHI, RefQuote, blend_mid, to_float
from susq.mapping.races import DEM, IND, REP, RaceKey, parse_party, parse_race

log = logging.getLogger(__name__)

BASE_URL = "https://api.elections.kalshi.com/trade-api/v2"
_SERIES = re.compile(
    r"^(?:KX)?(?:SENATE[A-Z]{2}S?|GOVPARTY[A-Z]{2}|HOUSE[A-Z]{2}(?:\d{1,2}|AL)|HOUSERACE|CONTROL[HS])$"
)
_CYCLE_SUFFIX = re.compile(r"-(?:26|2026|26NOV)$")
# Events whose ticker says another year but whose race is on the 2026 ballot.
CYCLE_OVERRIDES = frozenset({"GOVPARTYKS-27"})
_TICKER_STATE = re.compile(r"^(?:KX)?(?:SENATE|GOVPARTY|HOUSE(?:RACE-)?)([A-Z]{2})")
_PARTY_SUFFIX = {"D": DEM, "R": REP}


@dataclass(frozen=True)
class KalshiContract:
    """A discovered Kalshi market mapped to a race and party."""

    ticker: str
    event_ticker: str
    title: str
    race: RaceKey
    party: str
    warning: str = ""


def is_cycle_event(event_ticker: str, series_ticker: str) -> bool:
    """True for this cycle's race-winner and chamber-control events."""
    if not _SERIES.match(series_ticker):
        return False
    return bool(_CYCLE_SUFFIX.search(event_ticker)) or event_ticker in CYCLE_OVERRIDES


def classify(event: dict[str, Any], market: dict[str, Any]) -> KalshiContract | None:
    """Map one Kalshi market to (race, party), or None if it is not a clean party-winner contract."""
    ticker = str(market.get("ticker", ""))
    title = str(market.get("title") or "")
    race = parse_race(title) or parse_race(str(event.get("title") or ""))
    if race is None and event.get("series_ticker") in {"CONTROLH", "CONTROLS"}:
        race = parse_race(f"win the {'House' if event['series_ticker'] == 'CONTROLH' else 'Senate'}")
    if race is None:
        return None
    suffix = ticker.rsplit("-", 1)[-1]
    party = _PARTY_SUFFIX.get(suffix) or parse_party(title)
    if party not in {DEM, REP, IND}:
        return None
    warning = ""
    if (
        (m := _TICKER_STATE.match(str(event.get("event_ticker", ""))))
        and race.state
        and m.group(1) != race.state
    ):
        warning = f"ticker state {m.group(1)} != title state {race.state}"
    return KalshiContract(ticker, str(event.get("event_ticker", "")), title, race, party, warning)


def iter_open_events(client: PoliteClient, *, max_pages: int = 200) -> Iterator[dict[str, Any]]:
    """Every open event (with nested markets), following the cursor."""
    cursor = ""
    for _ in range(max_pages):
        params: dict[str, Any] = {"limit": 200, "status": "open", "with_nested_markets": "true"}
        if cursor:
            params["cursor"] = cursor
        page = client.get_json("/events", params)
        events = page.get("events") or []
        yield from events
        cursor = page.get("cursor") or ""
        if not cursor or not events:
            return
    log.warning("kalshi discovery stopped at max_pages=%d", max_pages)


def discover(client: PoliteClient) -> list[KalshiContract]:
    """All 2026 party-winner and chamber-control contracts currently open on Kalshi."""
    out: list[KalshiContract] = []
    for event in iter_open_events(client):
        if not is_cycle_event(str(event.get("event_ticker", "")), str(event.get("series_ticker", ""))):
            continue
        for market in event.get("markets") or []:
            if (c := classify(event, market)) is not None:
                out.append(c)
                if c.warning:
                    log.warning("kalshi %s: %s", c.ticker, c.warning)
    return out


def fetch_markets(client: PoliteClient, tickers: Iterable[str], *, batch: int = 100) -> list[dict[str, Any]]:
    """Current market objects for `tickers` via `GET /markets?tickers=` (comma-separated batches)."""
    tickers = list(tickers)
    out: list[dict[str, Any]] = []
    for i in range(0, len(tickers), batch):
        chunk = tickers[i : i + batch]
        page = client.get_json("/markets", {"tickers": ",".join(chunk), "limit": 1000})
        out.extend(page.get("markets") or [])
    return out


def raw_row(market: dict[str, Any], contract: KalshiContract, ts_ns: int) -> dict[str, Any]:
    """The archived raw row (one per market per snapshot)."""
    return {
        "ts_ns": ts_ns,
        "event_ticker": contract.event_ticker,
        "ticker": contract.ticker,
        "title": contract.title,
        "status": market.get("status"),
        "yes_bid": to_float(market.get("yes_bid_dollars")),
        "yes_ask": to_float(market.get("yes_ask_dollars")),
        "last": to_float(market.get("last_price_dollars")),
        "yes_bid_size": to_float(market.get("yes_bid_size_fp")),
        "yes_ask_size": to_float(market.get("yes_ask_size_fp")),
        "volume": to_float(market.get("volume_fp")),
        "volume_24h": to_float(market.get("volume_24h_fp")),
        "open_interest": to_float(market.get("open_interest_fp")),
        "race_id": contract.race.race_id,
        "party": contract.party,
    }


def to_quote(market: dict[str, Any], contract: KalshiContract, ts_ns: int) -> RefQuote | None:
    """Normalize to a reference quote; None when the market is not active or has no price."""
    if market.get("status") not in {"active", "open"}:
        return None
    bid, ask = to_float(market.get("yes_bid_dollars")), to_float(market.get("yes_ask_dollars"))
    last = to_float(market.get("last_price_dollars"))
    mid = blend_mid(bid, ask, last)
    if mid is None:
        return None
    return RefQuote(
        venue=KALSHI,
        race_id=contract.race.race_id,
        kind=contract.race.kind,
        party=contract.party,
        venue_market_id=contract.ticker,
        title=contract.title,
        bid=bid,
        ask=ask,
        last=last,
        mid=mid[0],
        spread=mid[1],
        volume=to_float(market.get("volume_fp")) or 0.0,
        ts_ns=ts_ns,
    )
