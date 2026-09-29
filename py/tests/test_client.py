"""Client behaviour that doesn't need the mock exchange: transport, errors, journaling, chunking."""

from __future__ import annotations

from collections.abc import Callable
from datetime import UTC, datetime, timedelta

import httpx
import orjson
import pytest
from susq.platform.client import HttpRecord, SuperMarketClient, parse_retry_after
from susq.platform.errors import ApiError, NetworkError, RetryClass
from susq.platform.models import OrderRequest

SECRET = "mock-key-do-not-journal-0123456789"


def make(
    handler: Callable[[httpx.Request], httpx.Response], **kw: object
) -> tuple[SuperMarketClient, list[HttpRecord]]:
    records: list[HttpRecord] = []
    client = SuperMarketClient(
        "https://sig.example/api/v1",
        SECRET,
        transport=httpx.MockTransport(handler),
        journal=records.append,
        **kw,  # type: ignore[arg-type]
    )
    return client, records


def ok(data: object, status: int = 200, headers: dict[str, str] | None = None) -> httpx.Response:
    return httpx.Response(
        status, content=orjson.dumps(data), headers={"x-request-id": "req-1", **(headers or {})}
    )


async def test_bearer_auth_and_base_url_path_join() -> None:
    seen: list[httpx.Request] = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req)
        return ok({"id": "p", "username": "u", "balance": 1.0})

    client, _ = make(handler)
    await client.get_account()
    assert str(seen[0].url) == "https://sig.example/api/v1/account"
    assert seen[0].headers["authorization"] == f"Bearer {SECRET}"


async def test_x_api_key_scheme() -> None:
    seen: list[httpx.Request] = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req)
        return ok({"id": "p"})

    client, _ = make(handler, auth_scheme="x-api-key")
    await client.get_account()
    assert seen[0].headers["x-api-key"] == SECRET
    assert "authorization" not in seen[0].headers


async def test_journal_never_contains_the_api_key() -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        return ok({"data": [], "pagination": {"hasMore": False}})

    client, records = make(handler)
    await client.list_orders("t-1", exchange_id="9")
    assert [r.direction for r in records] == ["request", "response"]
    assert records[0].correlation_id == records[1].correlation_id
    assert records[0].path == "/api/v1/orders?tournamentId=t-1&status=open&exchangeId=9&limit=200"
    assert records[1].request_id == "req-1"
    for r in records:
        assert SECRET not in repr(r)
    assert SECRET not in repr(client)


async def test_realtime_token_is_redacted_in_the_journal() -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        return ok(
            {
                "token": "jwt-secret",
                "expiresAt": "2026-10-01T19:00:00Z",
                "supabaseUrl": "https://x",
                "anonKey": "anon-secret",
                "channels": {"user": "user:1"},
            }
        )

    client, records = make(handler)
    tok = await client.mint_realtime_token()
    assert tok.token == "jwt-secret"
    assert "jwt-secret" not in records[-1].body_json
    assert "anon-secret" not in records[-1].body_json


async def test_error_envelope_is_parsed() -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        return ok(
            {"error": {"code": "RATE_LIMITED", "message": "slow", "details": {"x": 1}}},
            429,
            {"retry-after": "3"},
        )

    client, records = make(handler)
    with pytest.raises(ApiError) as info:
        await client.get_account()
    err = info.value
    assert (err.status, err.code, err.details, err.retry_after_s) == (429, "RATE_LIMITED", {"x": 1}, 3.0)
    assert err.request_id == "req-1"
    assert err.retry_class is RetryClass.RETRY_SAME_KEY
    assert records[-1].direction == "error"


async def test_non_json_error_bodies_become_http_codes() -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        return httpx.Response(504, text="<html>gateway timeout</html>")

    client, _ = make(handler)
    with pytest.raises(ApiError) as info:
        await client.get_account()
    assert info.value.code == "HTTP_504"
    assert info.value.retry_class is RetryClass.RETRY_READ


async def test_network_errors_are_their_own_class() -> None:
    def handler(req: httpx.Request) -> httpx.Response:
        raise httpx.ConnectTimeout("timed out", request=req)

    client, records = make(handler)
    with pytest.raises(NetworkError) as info:
        await client.get_account()
    assert info.value.retry_class is RetryClass.NETWORK
    assert records[-1].direction == "error"
    assert "ConnectTimeout" in records[-1].body_json


async def test_success_with_garbage_body_is_an_error() -> None:
    client, _ = make(lambda req: httpx.Response(200, text="not json"))
    with pytest.raises(ApiError, match="INVALID_RESPONSE"):
        await client.get_account()


async def test_prices_are_chunked_at_100_and_keep_order() -> None:
    calls: list[list[str]] = []

    def handler(req: httpx.Request) -> httpx.Response:
        ids = req.url.params["ids"].split(",")
        calls.append(ids)
        return ok(
            {
                "data": [
                    {"exchangeId": i, "latestPrice": None, "bestBid": None, "bestAsk": None}
                    for i in ids
                    if i != "7"
                ],
                "missingIds": [i for i in ids if i == "7"],
            }
        )

    client, _ = make(handler)
    ids = [str(i) for i in range(1, 251)]
    res = await client.get_prices(ids, "t")
    assert [len(c) for c in calls] == [100, 100, 50]
    assert [p.exchange_id for p in res.data] == [i for i in ids if i != "7"]
    assert res.missing_ids == ["7"]


async def test_cursor_iteration_follows_next_cursor() -> None:
    pages = {
        None: {"data": [{"id": "1", "title": "a"}], "pagination": {"hasMore": True, "nextCursor": "c2"}},
        "c2": {"data": [{"id": "2", "title": "b"}], "pagination": {"hasMore": False, "nextCursor": None}},
    }

    def handler(req: httpx.Request) -> httpx.Response:
        return ok(pages[req.url.params.get("cursor")])

    client, _ = make(handler)
    assert [m.id async for m in client.iter_tournament_markets("s")] == ["1", "2"]


async def test_batch_returns_207_and_raises_on_502() -> None:
    responses = iter(
        [
            ok(
                {
                    "results": [
                        {"index": 0, "ok": True, "status": 200, "data": {"orderId": 1, "exchangeId": "9"}}
                    ]
                },
                207,
            ),
            ok({"error": {"code": "ORDER_STATUS_UNKNOWN", "message": "?"}}, 502),
        ]
    )
    client, _ = make(lambda req: next(responses))
    req = OrderRequest("9", "yes", "buy", 1, 5000, datetime.now(UTC) + timedelta(minutes=1), "t")
    res = await client.place_batch([req], "k")
    assert res.http_status == 207
    with pytest.raises(ApiError) as info:
        await client.place_batch([req], "k")
    assert info.value.code == "ORDER_STATUS_UNKNOWN"


async def test_idempotency_key_in_body_and_journal() -> None:
    bodies: list[dict[str, object]] = []

    def handler(req: httpx.Request) -> httpx.Response:
        bodies.append(orjson.loads(req.content))
        return ok({"orderId": 5, "exchangeId": "9", "open": True, "quantityTraded": 0, "totalCost": 0})

    client, records = make(handler)
    req = OrderRequest("9", "no", "buy", 3, 450, datetime(2026, 10, 1, 16, 5, tzinfo=UTC), "t")
    await client.place_order(req, "key-1")
    assert bodies[0]["idempotencyKey"] == "key-1"
    assert bodies[0]["expirationDate"] == "2026-10-01T16:05:00Z"
    assert records[0].idempotency_key == "key-1"


async def test_cancel_all_rejects_both_scopes_locally() -> None:
    client, _ = make(lambda req: ok({}))
    with pytest.raises(ValueError, match="mutually exclusive"):
        await client.cancel_all("t", exchange_id="1", market_id="2")


def test_parse_retry_after() -> None:
    assert parse_retry_after("2") == 2.0
    assert parse_retry_after(None) is None
    assert parse_retry_after("garbage") is None
    assert (
        parse_retry_after(
            "Wed, 01 Oct 2026 16:00:10 GMT", now=datetime(2026, 10, 1, 16, 0, tzinfo=UTC).timestamp()
        )
        == 10.0
    )
