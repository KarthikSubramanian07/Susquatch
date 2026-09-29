"""Shared offline fixtures: real Kalshi/Polymarket captures (trimmed) and a realistic Cup listing."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from susq.collectors import kalshi, polymarket
from susq.collectors.quotes import RefQuote
from susq.mapping.cup import CupMarket, parse_markets
from susq.research_env import Paths

FIXTURES = Path(__file__).parent / "fixtures"
NOW_NS = 1_790_640_000 * 10**9  # 2026-09-29T00:00Z, just after the fixtures were captured


def load(name: str) -> object:
    return json.loads((FIXTURES / name).read_text())


def kalshi_quotes(ts_ns: int = NOW_NS) -> list[RefQuote]:
    events = [e for page in load("kalshi_events_pages.json") for e in page["events"]]
    contracts = {
        c.ticker: c
        for e in events
        if kalshi.is_cycle_event(e["event_ticker"], e["series_ticker"])
        for m in e["markets"]
        if (c := kalshi.classify(e, m))
    }
    markets = load("kalshi_markets.json")["markets"]
    return [
        q
        for m in markets
        if m["ticker"] in contracts and (q := kalshi.to_quote(m, contracts[m["ticker"]], ts_ns))
    ]


def polymarket_quotes(ts_ns: int = NOW_NS) -> list[RefQuote]:
    events = load("polymarket_events.json")
    contracts = [
        c
        for e in events
        if polymarket.is_race_event(e["slug"])
        for m in e["markets"]
        if (c := polymarket.classify(e, m))
    ]
    books = {b["asset_id"]: polymarket.top_of_book(b) for b in load("polymarket_books.json")}
    return [q for c in contracts if (q := polymarket.to_quote(c, books.get(c.yes_token), ts_ns))]


@pytest.fixture(scope="session")
def quotes() -> list[RefQuote]:
    return kalshi_quotes() + polymarket_quotes()


@pytest.fixture(scope="session")
def cup_markets() -> list[CupMarket]:
    return parse_markets(load("cup_markets.json"))


@pytest.fixture
def home(tmp_path: Path) -> Paths:
    """An empty $SUSQ_HOME with the repo's real config copied in."""
    (tmp_path / "config").mkdir()
    repo_cfg = Path(__file__).resolve().parents[3] / "config" / "susq.toml"
    (tmp_path / "config" / "susq.toml").write_text(repo_cfg.read_text())
    return Paths(tmp_path)
