"""The mock over HTTP through the real client: auth, faults, idempotency, batch, multi-leg, portfolio."""

from __future__ import annotations

import itertools

import httpx
import pytest
from susq.mock import MockConfig, create_app
from susq.mock.testing import KEY, SLUG, MockEnv
from susq.platform.errors import ApiError
from susq.platform.prices import to_pips

OH_D, OH_R = "1301", "1302"


async def best(env: MockEnv, ex: str) -> tuple[int, int]:
    book = await env.client.get_orderbook(ex, env.tid)
    return to_pips(book.bids[0].price), to_pips(book.asks[0].price)


async def clean_exchange(env: MockEnv) -> str:
    """An exchange whose top of book is the Sponsor (no stale student orders)."""
    state = await env.state()
    for m in state["markets"]:
        ex = m["exchangeId"]
        bid, ask = await best(env, ex)
        fair = to_pips(state["fair"][ex])
        if bid < fair < ask and 1000 < fair < 9000:
            return str(ex)
    raise AssertionError("no clean exchange")


# ── auth, scoping ───────────────────────────────────────────────────────────────────────────


async def test_auth_errors(env: MockEnv) -> None:
    r = await env.admin.get("/api/v1/account")
    assert (r.status_code, r.json()["error"]["code"]) == (401, "MISSING_API_KEY")
    r = await env.admin.get("/api/v1/account", headers={"Authorization": "Bearer mock-key-nope"})
    assert (r.status_code, r.json()["error"]["code"]) == (401, "INVALID_API_KEY")
    r = await env.admin.get("/api/v1/account", headers={"X-API-Key": KEY})
    assert r.status_code == 200
    assert r.headers["x-request-id"]


async def test_omitted_tournament_id_is_a_hard_error(env: MockEnv) -> None:
    r = await env.admin.get("/api/v1/exchanges/prices?ids=1301", headers={"Authorization": f"Bearer {KEY}"})
    assert r.status_code == 400
    assert "tournamentId" in r.json()["error"]["details"]


async def test_tournament_and_markets(env: MockEnv) -> None:
    t = await env.client.get_tournament(SLUG)
    assert t.id == env.tid
    assert t.market_count == 19
    assert t.member_count is not None
    assert t.member_count > 40
    page = await env.client.list_tournament_markets(SLUG, limit=5)
    assert len(page.data) == 5
    assert page.pagination.has_more
    assert page.pagination.total == 19
    everything = [m async for m in env.client.iter_tournament_markets(SLUG)]
    assert len(everything) == 19
    assert all(m.exchanges[0].latest_price is not None for m in everything)
    detail = await env.client.get_tournament_market(SLUG, "301")
    assert detail.exchanges[0].bids
    assert detail.exchanges[0].asks
    nodes = await env.client.get_market_nodes("315", env.tid)
    assert nodes.root["contractDetails"]["state"] == "NE"
    with pytest.raises(ApiError) as info:
        await env.client.get_tournament("nope")
    assert info.value.status == 404


async def test_prices_book_and_bad_cursor(env: MockEnv) -> None:
    prices = await env.client.get_prices([OH_D, "99999", OH_R], env.tid)
    assert [p.exchange_id for p in prices.data] == [OH_D, OH_R]
    assert prices.missing_ids == ["99999"]
    book = await env.client.get_orderbook(OH_D, env.tid, depth=5)
    assert [b.price for b in book.bids] == sorted((b.price for b in book.bids), reverse=True)
    assert [a.price for a in book.asks] == sorted(a.price for a in book.asks)
    r = await env.admin.get(
        f"/api/v1/tournaments/{SLUG}/markets?cursor=%%%", headers={"Authorization": f"Bearer {KEY}"}
    )
    assert r.json()["error"]["code"] == "INVALID_CURSOR"


# ── orders & idempotency ────────────────────────────────────────────────────────────────────


async def test_off_tick_order_is_400(env: MockEnv) -> None:
    body = {**env.order(OH_D).to_body(), "price": 0.123}
    with pytest.raises(ApiError) as info:
        await env.client.place_order_body(body, "k-off-tick")
    assert (info.value.status, info.value.code) == (400, "VALIDATION_ERROR")


async def test_same_key_replays_and_different_payload_conflicts(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    bid, _ = await best(env, ex)
    req = env.order(ex, price_pips=bid - 100)
    a = await env.client.place_order(req, "k-1")
    b = await env.client.place_order(req, "k-1")
    assert a.order_id == b.order_id
    assert len(await env.client.list_open_orders(env.tid, exchange_id=ex)) == 1
    with pytest.raises(ApiError) as info:
        await env.client.place_order(env.order(ex, qty=11, price_pips=bid - 100), "k-1")
    assert (info.value.status, info.value.code) == (409, "CONFLICT")


async def test_order_status_unknown_was_placed_and_retry_replays(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    bid, _ = await best(env, ex)
    await env.post_admin("/_mock/faults", {"order_status_unknown_next": 1})
    req = env.order(ex, price_pips=bid - 100)
    with pytest.raises(ApiError) as info:
        await env.client.place_order(req, "k-502")
    assert info.value.code == "ORDER_STATUS_UNKNOWN"
    open_orders = await env.client.list_open_orders(env.tid, exchange_id=ex)
    assert len(open_orders) == 1  # it WAS placed
    again = await env.client.place_order(req, "k-502")
    assert again.order_id == open_orders[0].id
    assert len(await env.client.list_open_orders(env.tid, exchange_id=ex)) == 1


async def test_503_and_in_flight_do_not_execute(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    bid, _ = await best(env, ex)
    await env.post_admin("/_mock/faults", {"service_unavailable_next": 1, "request_in_flight_next": 1})
    req = env.order(ex, price_pips=bid - 100)
    with pytest.raises(ApiError) as first:
        await env.client.place_order(req, "k-x")
    assert first.value.code == "REQUEST_IN_FLIGHT"
    with pytest.raises(ApiError) as second:
        await env.client.place_order(req, "k-x")
    assert second.value.code == "SERVICE_UNAVAILABLE"
    assert await env.client.list_open_orders(env.tid) == []
    placed = await env.client.place_order(req, "k-x")
    assert placed.open


async def test_rate_limit_sends_retry_after(env: MockEnv) -> None:
    await env.post_admin("/_mock/faults", {"rate_limit_rps": 2, "retry_after_s": 4})
    await env.client.get_account()
    await env.client.get_account()
    with pytest.raises(ApiError) as info:
        await env.client.get_account()
    assert (info.value.status, info.value.code, info.value.retry_after_s) == (429, "RATE_LIMITED", 4.0)


async def test_random_503(env: MockEnv) -> None:
    await env.post_admin("/_mock/faults", {"random_503_prob": 1.0})
    with pytest.raises(ApiError) as info:
        await env.client.get_account()
    assert info.value.status == 503


async def test_unknown_fault_field_is_rejected(env: MockEnv) -> None:
    r = await env.admin.post("/_mock/faults", json={"nonsense": 1})
    assert r.status_code == 400


async def test_taking_the_offer_moves_positions_pnl_and_tape(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    _, ask = await best(env, ex)
    res = await env.client.place_order(env.order(ex, qty=7, price_pips=ask), "k-take")
    assert res.quantity_traded == 7
    assert res.fill_price == pytest.approx(ask / 10_000)
    positions = await env.client.get_positions(SLUG)
    pos = next(p for p in positions.positions if p.exchange_id == ex)
    assert pos.quantity == 7
    assert pos.current_price == pytest.approx(ask / 10_000)  # valuation = last tournament trade
    assert pos.lots[0].entry_price == pytest.approx(ask / 10_000)
    pnl = await env.client.get_pnl(SLUG)
    assert pnl.total_account_value == pytest.approx(100_000, abs=1e-6)
    tape = await env.client.get_trades(ex, env.tid)
    assert tape.data[0].size == 7
    fills = await env.client.get_fills(SLUG)
    assert fills.data[0].order_id == res.order_id
    order_fills = await env.client.get_order_fills(res.order_id or 0)
    assert order_fills.total_quantity_filled == 7
    history = await env.client.get_price_history(ex, env.tid, resolution="1m")
    assert history.candles[-1].volume >= 7


async def test_insufficient_balance(env: MockEnv) -> None:
    with pytest.raises(ApiError) as info:
        await env.client.place_order(env.order(OH_D, qty=1_000_000, price_pips=5_000), "k-big")
    assert info.value.status == 400
    assert info.value.details["reason"] == "insufficient_balance"


# ── batch & multi-leg ───────────────────────────────────────────────────────────────────────


async def test_batch_statuses(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    bid, _ = await best(env, ex)
    good = env.order(ex, price_pips=bid - 100)
    huge = env.order(ex, qty=1_000_000, price_pips=bid - 100)
    ok = await env.client.place_batch([good], "b-ok")
    assert ok.http_status == 200
    mixed = await env.client.place_batch([env.order(ex, qty=2, price_pips=bid - 150), huge], "b-mixed")
    assert mixed.http_status == 207
    assert [r.ok for r in mixed.results] == [True, False]
    failed = await env.client.place_batch([huge], "b-fail")
    assert failed.http_status == 422
    replay = await env.client.place_batch([good], "b-ok")
    assert replay.results[0].data["orderId"] == ok.results[0].data["orderId"]
    body = {"idempotencyKey": "b-bad", "orders": [good.to_body(), {**good.to_body(), "price": 0.123}]}
    r = await env.admin.post("/api/v1/orders/batch", json=body, headers={"Authorization": f"Bearer {KEY}"})
    assert r.status_code == 400
    assert len(await env.client.list_open_orders(env.tid)) == 2  # nothing from the rejected batch


async def test_multi_leg_is_atomic(env: MockEnv) -> None:
    b1, _ = await best(env, OH_D)
    b2, _ = await best(env, OH_R)
    legs = [env.order(OH_D, price_pips=b1 - 100), env.order(OH_R, qty=1_000_000, price_pips=b2 - 100)]
    with pytest.raises(ApiError) as info:
        await env.client.place_multi_leg(legs, "ml-1")
    assert info.value.details["leg"] == 1
    assert await env.client.list_open_orders(env.tid) == []
    res = await env.client.place_multi_leg([legs[0], env.order(OH_R, price_pips=b2 - 100)], "ml-2")
    assert len(res.results) == 2
    assert len(await env.client.list_open_orders(env.tid)) == 2


async def test_multi_leg_validation(env: MockEnv) -> None:
    leg = env.order(OH_D, price_pips=1_000)
    r = await env.admin.post(
        "/api/v1/orders/multi-leg",
        json={"idempotencyKey": "d", "legs": [leg.to_body(), leg.to_body()]},
        headers={"Authorization": f"Bearer {KEY}"},
    )
    assert r.status_code == 400
    with pytest.raises(ApiError) as info:
        await env.client.place_multi_leg([leg], "ml-rc", relationship_constraint="not-a-relationship")
    assert (info.value.status, info.value.code) == (422, "RELATIONSHIP_VIOLATION")


# ── cancels & expiry ────────────────────────────────────────────────────────────────────────


async def test_cancel_and_cancel_all(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    bid, _ = await best(env, ex)
    orders = [
        await env.client.place_order(env.order(ex, price_pips=bid - 50 * (i + 2)), f"c-{i}") for i in range(4)
    ]
    cancelled = await env.client.cancel_order(orders[0].order_id or 0)
    assert cancelled.order_id == orders[0].order_id
    with pytest.raises(ApiError) as info:
        await env.client.cancel_order(orders[0].order_id or 0)
    assert info.value.code == "CONFLICT"
    with pytest.raises(ApiError) as info:
        await env.client.cancel_order(123456789)
    assert info.value.status == 404
    await env.post_admin("/_mock/faults", {"cancel_unavailable_next": 1})
    with pytest.raises(ApiError) as info:
        await env.client.cancel_order(orders[1].order_id or 0)
    assert info.value.status == 503
    await env.post_admin("/_mock/faults", {"cancel_all_paused_next": 1})
    with pytest.raises(ApiError) as info:
        await env.client.cancel_all(env.tid, exchange_id=ex)
    assert info.value.status == 503
    assert info.value.details == {"cancelled": 1, "errors": [], "remaining": 2}
    done = await env.client.cancel_all(env.tid, exchange_id=ex)
    assert done.complete
    assert done.cancelled == 2
    assert await env.client.list_open_orders(env.tid) == []


async def test_expired_orders_leave_open_and_list_as_expired(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    bid, _ = await best(env, ex)
    res = await env.client.place_order(env.order(ex, price_pips=bid - 100, ttl_s=30), "e-1")
    await env.post_admin("/_mock/clock", {"advanceSeconds": 31})
    assert await env.client.list_open_orders(env.tid) == []
    expired = await env.client.list_orders(env.tid, status="expired")
    assert [o.id for o in expired.data] == [res.order_id]
    closed = await env.client.list_orders(env.tid, status="closed")
    assert res.order_id in [o.id for o in closed.data]


# ── leaderboard, relationships, realtime token ──────────────────────────────────────────────


async def test_leaderboard_ranks_and_my_rank(env: MockEnv) -> None:
    lb = await env.client.get_leaderboard(SLUG)
    assert lb.my_rank is None  # not ranked before the first trade
    before = lb.total
    ex = await clean_exchange(env)
    _, ask = await best(env, ex)
    await env.client.place_order(env.order(ex, qty=1, price_pips=ask), "lb-1")
    lb = await env.client.get_leaderboard(SLUG, limit=100)
    assert lb.total == before + 1
    assert lb.my_rank is not None
    entries = lb.leaderboard
    for e in entries:
        first_equal = next(j for j, f in enumerate(entries) if f.pnl == e.pnl)
        assert e.rank == first_equal + 1  # competition ranking 1-1-3
    assert any(a.rank == b.rank for a, b in itertools.pairwise(entries))


async def test_relationships_and_constraint_violation(env: MockEnv) -> None:
    rels = [r async for r in env.client.iter_relationships(env.tid)]
    kinds = {(r.type, r.is_exhaustive) for r in rels}
    assert ("mutually_exclusive", True) in kinds
    assert ("mutually_exclusive", False) in kinds  # Nebraska
    assert ("implication", False) in kinds
    assert (await env.client.get_constraints(env.tid)).data == []
    out = await env.post_admin("/_mock/violation", {"amount": 0.06})
    res = await env.client.get_constraints(env.tid)
    assert [e.relationship_id for e in res.data] == [out["relationshipId"]]
    ev = res.data[0]
    assert ev.violation_amount >= 0.06
    assert {t.outcome_side for t in ev.suggested_corrective_trades} == {"No"}
    assert sum(p.price for p in ev.observed_prices or []) == pytest.approx(1 + ev.violation_amount)
    implication = next(r for r in rels if r.type == "implication")
    out = await env.post_admin("/_mock/violation", {"relationshipId": implication.id, "amount": 0.05})
    res = await env.client.get_constraints(env.tid, relationship_id=implication.id)
    sides = {(t.exchange_id, t.outcome_side) for t in res.data[0].suggested_corrective_trades}
    ante, cons = implication.exchange_ids()
    assert sides == {(ante, "No"), (cons, "Yes")}


async def test_realtime_token(env: MockEnv) -> None:
    tok = await env.client.mint_realtime_token()
    assert tok.channels["user"].startswith("user:")
    assert (tok.expires_at - env.mx.clock.now()).total_seconds() == pytest.approx(3 * 3600)


# ── scenario admin & determinism ────────────────────────────────────────────────────────────


async def test_fair_move_and_stale_orders(env: MockEnv) -> None:
    ex = await clean_exchange(env)
    await env.post_admin("/_mock/fair", {"exchangeId": ex, "fair": 0.30})
    bid, ask = await best(env, ex)
    assert bid <= 2_900
    assert ask >= 3_100
    out = await env.post_admin("/_mock/stale-order", {"exchangeId": ex, "side": "bid", "through": 0.05})
    bid, _ = await best(env, ex)
    assert bid == 3_500
    assert out["orderId"] > 0


async def test_same_seed_same_world_and_different_seed_differs(mock_config: MockConfig) -> None:
    async def snapshot(seed: int) -> dict:  # type: ignore[type-arg]
        app = create_app(MockConfig(seed=seed, frozen_now=mock_config.frozen_now))
        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://m") as c:
            state = (await c.get("/_mock/state")).json()
            book = (
                await c.get(
                    f"/api/v1/exchanges/{OH_D}/orderbook?tournamentId={state['tournamentId']}",
                    headers={"Authorization": f"Bearer {KEY}"},
                )
            ).json()
            lb = (
                await c.get(
                    f"/api/v1/tournaments/{SLUG}/leaderboard", headers={"Authorization": f"Bearer {KEY}"}
                )
            ).json()
        return {"fair": state["fair"], "book": book, "lb": lb}

    assert await snapshot(7) == await snapshot(7)
    assert (await snapshot(7))["fair"] != (await snapshot(8))["fair"]
