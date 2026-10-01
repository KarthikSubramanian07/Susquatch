"""Realtime subscriptions: per-market topics for the most liquid markets, capped, demuxed on exchangeId."""

from __future__ import annotations

from susq.platform.models import PriceSnapshot, RealtimeToken
from susq.platform.realtime import BroadcastMeta
from susq_bridge.runtime import REALTIME_MARKET_CHANNELS, Bridge

from .conftest import Harness


async def test_subscribes_per_market_topics_not_the_tournament_topic(live_bridge: Bridge) -> None:
    tid = live_bridge.tournament.id
    token = RealtimeToken(
        token="t",
        expires_at=live_bridge.clock(),
        supabase_url="http://x",
        anon_key="a",
        channels={"user": "user:me"},
    )
    rt = live_bridge.build_realtime(token)
    channels = rt._channels
    assert f"tournament:{tid}" not in channels  # no market updates there since 2026-10-01
    assert channels[0] == f"relationships:violations:{tid}"
    assert channels[-1] == "user:me"
    market_topics = [c for c in channels if c.startswith(f"tournament:{tid}:market:")]
    assert len(market_topics) == min(REALTIME_MARKET_CHANNELS, len(live_bridge.market.markets))
    assert set(market_topics) == {f"tournament:{tid}:market:{m}" for m in live_bridge.market.markets}
    for topic in market_topics:
        assert {e for c, e in rt._callbacks if c == topic} == {"market_batch", "book_dirty", "market_settled"}


async def test_cap_keeps_the_tightest_markets_first(live_bridge: Bridge) -> None:
    market = live_bridge.market
    prices = market.prices
    ex_a, ex_b, ex_c = sorted(prices)[:3]
    prices[ex_a] = PriceSnapshot(exchange_id=ex_a, best_bid=0.40, best_ask=0.60, spread=0.20)  # wide
    prices[ex_b] = PriceSnapshot(exchange_id=ex_b, best_bid=0.49, best_ask=0.50, spread=0.01)  # tight
    prices[ex_c] = PriceSnapshot(exchange_id=ex_c, best_bid=None, best_ask=0.50, spread=None)  # one-sided
    for ex in list(prices)[3:]:
        prices[ex] = PriceSnapshot(exchange_id=ex, best_bid=0.45, best_ask=0.55, spread=0.10)
    m_a, m_b, m_c = (market.exchange_market[ex] for ex in (ex_a, ex_b, ex_c))
    ranked = market.most_liquid_markets()
    assert ranked[0] == m_b
    assert ranked.index(m_a) > ranked.index(m_b)
    assert ranked[-2] == m_a  # widest two-sided quote beats the one-sided one
    assert ranked[-1] == m_c
    assert len(ranked) == len(market.markets)
    capped = live_bridge.market_channels(cap=2)
    assert capped == [f"tournament:{live_bridge.tournament.id}:market:{m}" for m in ranked[:2]]
    assert live_bridge.market_channels(cap=0) == []
    # A market with no price snapshot at all sorts after everything with a quote, even a one-sided one.
    del prices[ex_b]
    assert market.most_liquid_markets()[-2:] == [m_c, m_b]


async def test_market_events_invalidate_books_and_settlements_resync(
    harness: Harness, live_bridge: Bridge
) -> None:
    market = live_bridge.market
    ex = sorted(market.exchange_market)[0]
    mid = market.exchange_market[ex]
    topic = f"tournament:{live_bridge.tournament.id}:market:{mid}"
    await market.book(ex)
    assert ex in market._books
    meta = BroadcastMeta(topic, "market_batch", 0.0, 1)
    live_bridge._on_market_batch({"trades": [], "bookDirty": [{"exchangeId": ex, "marketId": mid}]}, meta)
    assert ex not in market._books
    await market.book(ex)
    live_bridge._on_market_batch(
        {"exchangeId": ex, "marketId": mid}, BroadcastMeta(topic, "book_dirty", 0, 2)
    )
    assert ex not in market._books
    before = list(live_bridge.oms.resync_reasons)
    live_bridge._on_market_settled(
        {"exchangeId": ex, "marketId": mid}, BroadcastMeta(topic, "market_settled", 0, 3)
    )
    assert live_bridge.oms.resync_reasons[len(before) :] == [f"market_settled:{topic}"]
    live_bridge._on_market_batch({"marketSettled": [{"exchangeId": ex}]}, meta)
    assert live_bridge.oms.resync_reasons[-1] == f"market_settled:{topic}"
