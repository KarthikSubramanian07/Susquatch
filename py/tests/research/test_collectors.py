"""Collectors against recorded venue responses (respx), storage, and the polite HTTP client."""

from __future__ import annotations

import json

import httpx
import polars as pl
import pytest
import respx
from susq.collectors import kalshi, polymarket
from susq.collectors.http import PoliteClient
from susq.collectors.quotes import KALSHI, POLYMARKET, blend_mid
from susq.collectors.run import ReferenceCollector, load_latest_quotes
from susq.collectors.store import read_latest, read_snapshots, write_snapshot

from .conftest import NOW_NS, load


def _no_sleep(_: float) -> None:
    return None


def _clients() -> tuple[PoliteClient, PoliteClient, PoliteClient]:
    return (
        PoliteClient(kalshi.BASE_URL, min_interval_s=0.0, sleep=_no_sleep),
        PoliteClient(polymarket.GAMMA_URL, min_interval_s=0.0, sleep=_no_sleep),
        PoliteClient(polymarket.CLOB_URL, min_interval_s=0.0, sleep=_no_sleep),
    )


def _mock_venues(router: respx.MockRouter) -> None:
    pages = load("kalshi_events_pages.json")

    def events(request: httpx.Request) -> httpx.Response:
        cursor = request.url.params.get("cursor", "")
        return httpx.Response(200, json=pages[1] if cursor == "page2" else pages[0])

    def markets(request: httpx.Request) -> httpx.Response:
        wanted = set(request.url.params["tickers"].split(","))
        rows = [m for m in load("kalshi_markets.json")["markets"] if m["ticker"] in wanted]
        return httpx.Response(200, json={"markets": rows, "cursor": ""})

    def books(request: httpx.Request) -> httpx.Response:
        wanted = {x["token_id"] for x in json.loads(request.content)}
        return httpx.Response(200, json=[b for b in load("polymarket_books.json") if b["asset_id"] in wanted])

    router.get(f"{kalshi.BASE_URL}/events").mock(side_effect=events)
    router.get(f"{kalshi.BASE_URL}/markets").mock(side_effect=markets)
    router.get(f"{polymarket.GAMMA_URL}/events").mock(
        return_value=httpx.Response(200, json=load("polymarket_events.json"))
    )
    router.post(f"{polymarket.CLOB_URL}/books").mock(side_effect=books)


@respx.mock
def test_kalshi_discovery_follows_cursor_and_filters_cycle() -> None:
    _mock_venues(respx.mock)
    k, _, _ = _clients()
    contracts = kalshi.discover(k)
    by_race = {(c.race.race_id, c.party): c for c in contracts}
    assert ("2026-CONTROL-HOUSE", "DEM") in by_race
    assert ("2026-CONTROL-SENATE", "REP") in by_race
    assert by_race[("2026-SEN-NE", "IND")].ticker.startswith("SENATENE-26-")
    assert ("2026-SEN-OH-S", "DEM") in by_race
    assert ("2026-GOV-KS", "REP") in by_race  # GOVPARTYKS-27 is on the 2026 ballot (override)
    assert ("2026-HOUSE-AL-01", "DEM") in by_race
    # Off-cycle (SENATENH-28) and non-winner (turnout) events are dropped.
    assert all(not c.event_ticker.endswith("-28") for c in contracts)
    assert all("TURN" not in c.event_ticker for c in contracts)
    # SENATELA-26 is titled Kentucky: the title wins and the disagreement is recorded.
    ky = by_race[("2026-SEN-KY", "REP")]
    assert ky.event_ticker == "SENATELA-26"
    assert "LA != title state KY" in ky.warning


@respx.mock
def test_kalshi_snapshot_normalizes_quotes() -> None:
    _mock_venues(respx.mock)
    k, _, _ = _clients()
    contracts = {c.ticker: c for c in kalshi.discover(k)}
    markets = kalshi.fetch_markets(k, contracts, batch=7)
    assert len(markets) == len(contracts)
    quotes = [q for m in markets if (q := kalshi.to_quote(m, contracts[m["ticker"]], NOW_NS))]
    nh = next(q for q in quotes if q.race_id == "2026-SEN-NH" and q.party == "DEM")
    assert nh.venue == KALSHI
    assert 0.7 < nh.mid < 0.95
    assert nh.bid <= nh.mid <= nh.ask
    assert all(0.0 < q.mid < 1.0 for q in quotes)


@respx.mock
def test_polymarket_discovery_drops_placeholders_and_non_winners() -> None:
    _mock_venues(respx.mock)
    _, g, c = _clients()
    contracts = polymarket.discover(g)
    races = {(x.race.race_id, x.party) for x in contracts}
    assert ("2026-SEN-NE", "IND") in races
    assert ("2026-CONTROL-HOUSE", "REP") in races
    assert ("2026-SEN-OH-S", "DEM") in races
    assert ("2026-HOUSE-AK-AL", "REP") in races
    assert not any("margin" in x.event_slug for x in contracts)
    assert not any("Person" in x.question or "Option" in x.question for x in contracts)
    books = polymarket.fetch_books(c, [x.yes_token for x in contracts], batch=5)
    assert set(books) == {x.yes_token for x in contracts}
    for tob in books.values():
        if tob.bid is not None and tob.ask is not None:
            assert tob.bid <= tob.ask  # best = max(bids), min(asks) whatever the level order


@respx.mock
def test_collect_once_writes_raw_and_reference_parquet(tmp_path) -> None:
    _mock_venues(respx.mock)
    k, g, c = _clients()
    col = ReferenceCollector(
        tmp_path, kalshi_client=k, gamma_client=g, clob_client=c, clock_ns=lambda: NOW_NS
    )
    res = col.collect_once()
    assert res.errors == {}
    assert {q.venue for q in res.quotes} == {KALSHI, POLYMARKET}
    assert len(res.files) == 3
    raw = read_latest(tmp_path / "kalshi")
    assert raw.schema["ts"] == pl.Datetime("us", "UTC")
    assert (tmp_path / "reference").rglob("*.parquet")
    back = load_latest_quotes(tmp_path)
    assert len(back) == len(res.quotes)
    assert back[0].ts_ns == NOW_NS
    assert all(p.suffix == ".parquet" and "date=2026-09-" in str(p) for p in res.files)


@respx.mock
def test_one_venue_down_does_not_stop_the_other(tmp_path) -> None:
    _mock_venues(respx.mock)
    respx.mock.get(f"{kalshi.BASE_URL}/events").mock(return_value=httpx.Response(403, json={}))
    k, g, c = _clients()
    col = ReferenceCollector(
        tmp_path, kalshi_client=k, gamma_client=g, clob_client=c, clock_ns=lambda: NOW_NS
    )
    res = col.collect_once()
    assert "kalshi" in res.errors
    assert res.quotes
    assert {q.venue for q in res.quotes} == {POLYMARKET}


@respx.mock
def test_clob_failure_falls_back_to_gamma_top_of_book(tmp_path) -> None:
    _mock_venues(respx.mock)
    respx.mock.post(f"{polymarket.CLOB_URL}/books").mock(return_value=httpx.Response(400, json={}))
    k, g, c = _clients()
    col = ReferenceCollector(
        tmp_path, kalshi_client=k, gamma_client=g, clob_client=c, clock_ns=lambda: NOW_NS
    )
    res = col.collect_once()
    assert "polymarket" not in res.errors
    raw = read_latest(tmp_path / "polymarket")
    assert set(raw["source"].to_list()) == {"gamma"}


@respx.mock
def test_polite_client_retries_429_honouring_retry_after() -> None:
    route = respx.get("https://example.test/x").mock(
        side_effect=[
            httpx.Response(429, headers={"Retry-After": "7"}),
            httpx.Response(503),
            httpx.Response(200, json={"ok": True}),
        ]
    )
    slept: list[float] = []
    client = PoliteClient("https://example.test", min_interval_s=0.0, sleep=slept.append)
    assert client.get_json("/x") == {"ok": True}
    assert route.call_count == 3
    assert slept[0] == 7.0
    assert slept[1] == 2.0


@respx.mock
def test_polite_client_does_not_retry_terminal_4xx() -> None:
    route = respx.get("https://example.test/x").mock(return_value=httpx.Response(404))
    client = PoliteClient("https://example.test", min_interval_s=0.0, sleep=_no_sleep)
    with pytest.raises(httpx.HTTPStatusError):
        client.get_json("/x")
    assert route.call_count == 1


def test_polite_client_paces_requests() -> None:
    now = [100.0]
    slept: list[float] = []

    def sleep(s: float) -> None:
        slept.append(s)
        now[0] += s

    transport = httpx.MockTransport(lambda r: httpx.Response(200, json={}))
    client = PoliteClient(
        "https://e.test", min_interval_s=0.5, transport=transport, sleep=sleep, clock=lambda: now[0]
    )
    for _ in range(3):
        client.get_json("/")
    assert slept == [0.5, 0.5]


@pytest.mark.parametrize(
    ("bid", "ask", "last", "expected"),
    [
        (0.85, 0.86, 0.86, (0.855, 0.01)),
        (0.0, 0.005, 0.005, (0.005, 0.2)),  # no bid: consistent last trade
        (None, 0.001, 0.999, (0.0005, 0.001)),  # Idaho: stale 99.9c print ignored
        (0.10, 0.60, 0.30, (0.30, 0.2)),  # too wide: last trade inside the book
        (0.10, 0.60, 0.90, None),  # too wide and last outside: no price
        (0.985, None, None, (0.9925, 0.015)),
        (None, None, None, None),
    ],
)
def test_blend_mid(bid, ask, last, expected) -> None:
    got = blend_mid(bid, ask, last)
    if expected is None:
        assert got is None
    else:
        assert got == pytest.approx(expected)


def test_store_roundtrip_and_date_partitions(tmp_path) -> None:
    rows = [{"ts_ns": NOW_NS, "a": 1.0}, {"ts_ns": NOW_NS, "a": None}]
    p1 = write_snapshot(tmp_path, rows, NOW_NS)
    p2 = write_snapshot(
        tmp_path, [{"ts_ns": NOW_NS + 86_400 * 10**9, "a": 2.0, "b": "x"}], NOW_NS + 86_400 * 10**9
    )
    assert p1 is not None
    assert p2 is not None
    assert p1.parent != p2.parent
    assert write_snapshot(tmp_path, [], NOW_NS) is None
    df = read_snapshots(tmp_path)
    assert df.height == 3
    assert "b" in df.columns
    assert read_latest(tmp_path)["a"].to_list() == [2.0]
    assert not list(tmp_path.rglob("*.tmp"))
