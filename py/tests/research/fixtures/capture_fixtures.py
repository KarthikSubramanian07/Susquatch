"""Re-capture the trimmed real-venue fixtures used by the offline collector tests.

Manual tool (hits public Kalshi/Polymarket endpoints, never runs in CI):

    uv run python py/tests/research/fixtures/capture_fixtures.py

It keeps a hand-picked set of events that exercise every parser branch (chamber control, special
elections, an independent, the SENATELA/Kentucky ticker quirk, an off-cycle event, House seats,
at-large seats, rejected margin/turnout events) and drops fields the code never reads.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from susq.collectors import kalshi, polymarket
from susq.collectors.http import PoliteClient

HERE = Path(__file__).parent
KALSHI_EVENTS = [
    "CONTROLH-2026", "CONTROLS-2026", "SENATENH-26", "SENATENE-26", "SENATEOHS-26", "SENATEME-26",
    "SENATETX-26", "SENATEIA-26", "SENATELA-26", "SENATEID-26", "SENATEAK-26", "SENATEMI-26",
    "GOVPARTYGA-26", "GOVPARTYKS-27", "HOUSENY17-26", "HOUSEPA7-26", "HOUSEAKAL-26",
    "KXHOUSERACE-AL01-26", "SENATENH-28", "KXMIDTERMVOTETURN-WYSEN",
]  # fmt: skip
POLY_EVENTS = [
    "which-party-will-win-the-senate-in-2026", "which-party-will-win-the-house-in-2026",
    "new-hampshire-senate-election-winner", "nebraska-senate-election-winner",
    "ohio-senate-election-winner", "maine-senate-election-winner", "texas-senate-election-winner",
    "iowa-senate-election-winner", "idaho-senate-election-winner", "michigan-senate-election-winner",
    "georgia-governor-winner-2026", "ny-17-house-election-winner", "ak-al-house-election-winner",
    "new-hampshire-senate-margin-of-victory-2026",
]  # fmt: skip
KALSHI_MARKET_FIELDS = (
    "ticker", "event_ticker", "title", "yes_sub_title", "status", "yes_bid_dollars", "yes_ask_dollars",
    "last_price_dollars", "yes_bid_size_fp", "yes_ask_size_fp", "volume_fp", "volume_24h_fp",
    "open_interest_fp",
)  # fmt: skip
POLY_MARKET_FIELDS = (
    "id", "question", "groupItemTitle", "outcomes", "outcomePrices", "clobTokenIds", "bestBid",
    "bestAsk", "lastTradePrice", "volumeNum", "active", "closed",
)  # fmt: skip


def _pick(d: dict[str, Any], keys: tuple[str, ...]) -> dict[str, Any]:
    return {k: d.get(k) for k in keys}


def capture_kalshi(client: PoliteClient) -> None:
    wanted = set(KALSHI_EVENTS)
    events = [e for e in kalshi.iter_open_events(client) if e.get("event_ticker") in wanted]
    trimmed = [
        {
            "event_ticker": e["event_ticker"],
            "series_ticker": e["series_ticker"],
            "title": e["title"],
            "category": e.get("category"),
            "markets": [_pick(m, KALSHI_MARKET_FIELDS) for m in e.get("markets") or []],
        }
        for e in events
    ]
    half = len(trimmed) // 2
    pages = [{"events": trimmed[:half], "cursor": "page2"}, {"events": trimmed[half:], "cursor": ""}]
    (HERE / "kalshi_events_pages.json").write_text(json.dumps(pages, indent=1))
    tickers = [m["ticker"] for e in trimmed for m in e["markets"]]
    markets = kalshi.fetch_markets(client, tickers)
    (HERE / "kalshi_markets.json").write_text(
        json.dumps({"markets": [_pick(m, KALSHI_MARKET_FIELDS) for m in markets], "cursor": ""}, indent=1)
    )


def capture_polymarket(gamma: PoliteClient, clob: PoliteClient) -> None:
    wanted = set(POLY_EVENTS)
    events = [e for e in polymarket.iter_events(gamma) if e.get("slug") in wanted]
    trimmed = []
    for e in events:
        markets = [m for m in e.get("markets") or [] if m.get("active") and not m.get("closed")]
        placeholder = next((m for m in e.get("markets") or [] if not m.get("active")), None)
        keep = markets + ([placeholder] if placeholder else [])
        trimmed.append(
            {"slug": e["slug"], "title": e["title"], "markets": [_pick(m, POLY_MARKET_FIELDS) for m in keep]}
        )
    (HERE / "polymarket_events.json").write_text(json.dumps(trimmed, indent=1))
    contracts = [c for e in trimmed for m in e["markets"] if (c := polymarket.classify(e, m))]
    books = clob.post_json("/books", [{"token_id": c.yes_token} for c in contracts])
    for b in books:
        b["bids"] = sorted(b.get("bids") or [], key=lambda x: float(x["price"]))[-3:]
        b["asks"] = sorted(b.get("asks") or [], key=lambda x: -float(x["price"]))[-3:]
        b.pop("hash", None)
    (HERE / "polymarket_books.json").write_text(json.dumps(books, indent=1))


def main() -> None:
    with (
        PoliteClient(kalshi.BASE_URL, min_interval_s=0.3) as k,
        PoliteClient(polymarket.GAMMA_URL, min_interval_s=0.3) as g,
        PoliteClient(polymarket.CLOB_URL, min_interval_s=0.3) as c,
    ):
        capture_kalshi(k)
        capture_polymarket(g, c)
    (HERE / "CAPTURED_AT.txt").write_text(datetime.now(UTC).isoformat() + "\n")


if __name__ == "__main__":
    main()
