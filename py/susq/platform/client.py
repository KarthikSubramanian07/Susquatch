"""Async client for The Super Market REST API (``/api/v1``), scoped to what Susquatch calls.

Design rules:

* **One attempt per call.** Every method sends exactly one HTTP request and either returns a parsed
  model or raises :class:`~susq.platform.errors.ApiError` / :class:`~susq.platform.errors.NetworkError`.
  Retries, backoff and rate limiting live in the bridge gateway, which knows whether a call is a read
  or a write and which idempotency key it carries.
* **Journal every exchange.** An optional ``journal`` hook receives an :class:`HttpRecord` for the
  request and for its response/error. The API key travels only in a header and is never recorded.
* **Explicit tournament scope.** Every tournament-sensitive method takes ``tournament_id`` (or the
  slug form). Keys are organization-bound, and an omitted ``tournamentId`` silently resolves to the
  organization's default tournament.
"""

from __future__ import annotations

import time
import uuid
from collections.abc import AsyncIterator, Callable, Iterable, Sequence
from dataclasses import dataclass
from email.utils import parsedate_to_datetime
from typing import Any, Literal, Protocol

import httpx
import orjson

from susq.platform.errors import ApiError, NetworkError
from susq.platform.models import (
    Account,
    BatchResult,
    BulkPrices,
    CancelAllResult,
    CancelResult,
    Collateral,
    ConstraintsResult,
    Fill,
    FillsPage,
    Leaderboard,
    Market,
    MarketNodes,
    MarketsPage,
    MultiLegResult,
    Order,
    OrderBook,
    OrderFills,
    OrderRequest,
    OrdersPage,
    PlaceOrderResult,
    PnL,
    Positions,
    PriceHistory,
    RealtimeToken,
    Relationship,
    RelationshipsPage,
    SettlementsPage,
    Tournament,
    TournamentsPage,
    TradesPage,
    TransactionsPage,
)

__all__ = [
    "MAX_BATCH_ORDERS",
    "MAX_MULTI_LEG",
    "MAX_PRICE_IDS",
    "HttpRecord",
    "JournalHook",
    "SuperMarketClient",
    "TradingApi",
    "parse_retry_after",
]

MAX_PRICE_IDS = 100
MAX_BATCH_ORDERS = 50
MAX_MULTI_LEG = 10


@dataclass(frozen=True, slots=True)
class HttpRecord:
    """One journal line's worth of an HTTP exchange (mirrors ``susq.v1.JournalEntry``)."""

    ts_ns: int
    direction: Literal["request", "response", "error"]
    method: str
    path: str
    body_json: str
    status: int
    latency_us: int
    request_id: str
    idempotency_key: str
    correlation_id: str


JournalHook = Callable[[HttpRecord], None]


class TradingApi(Protocol):
    """The write surface shared by the live client and the paper broker."""

    async def place_order(self, order: OrderRequest, idempotency_key: str) -> PlaceOrderResult: ...

    async def place_batch(self, orders: Sequence[OrderRequest], idempotency_key: str) -> BatchResult: ...

    async def place_multi_leg(
        self,
        legs: Sequence[OrderRequest],
        idempotency_key: str,
        relationship_constraint: str | None = None,
    ) -> MultiLegResult: ...

    async def cancel_order(self, order_id: int) -> CancelResult: ...

    async def cancel_all(
        self, tournament_id: str, *, exchange_id: str | None = None, market_id: str | None = None
    ) -> CancelAllResult: ...

    async def list_open_orders(
        self, tournament_id: str, *, exchange_id: str | None = None, market_id: str | None = None
    ) -> list[Order]: ...


def parse_retry_after(value: str | None, *, now: float | None = None) -> float | None:
    """``Retry-After`` as seconds; accepts delta-seconds or an HTTP-date."""
    if not value:
        return None
    value = value.strip()
    try:
        return max(0.0, float(value))
    except ValueError:
        pass
    try:
        when = parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    now = time.time() if now is None else now
    return max(0.0, when.timestamp() - now)


def _clean(params: dict[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for k, v in params.items():
        if v is None:
            continue
        out[k] = ("true" if v else "false") if isinstance(v, bool) else v
    return out


def _redact(text: str, fields: Sequence[str]) -> str:
    """Replace secret top-level JSON fields (e.g. the realtime JWT) before journaling."""
    try:
        data = orjson.loads(text)
    except orjson.JSONDecodeError:
        return text
    if not isinstance(data, dict):
        return text
    return orjson.dumps({k: ("[redacted]" if k in fields else v) for k, v in data.items()}).decode()


def _chunks[T](items: Sequence[T], size: int) -> Iterable[Sequence[T]]:
    for i in range(0, len(items), size):
        yield items[i : i + size]


class SuperMarketClient:
    """Thin, typed, single-attempt client. Use as an async context manager or call :meth:`aclose`."""

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        timeout_s: float = 10.0,
        journal: JournalHook | None = None,
        transport: httpx.AsyncBaseTransport | None = None,
        auth_scheme: Literal["bearer", "x-api-key"] = "bearer",
    ) -> None:
        if not api_key:
            raise ValueError("api_key is required")
        headers = {"Accept": "application/json", "User-Agent": "susquatch/0.1"}
        if auth_scheme == "bearer":
            headers["Authorization"] = f"Bearer {api_key}"
        else:
            headers["X-API-Key"] = api_key
        self.base_url = base_url.rstrip("/")
        self._journal = journal
        self._http = httpx.AsyncClient(
            base_url=self.base_url + "/",
            headers=headers,
            timeout=httpx.Timeout(timeout_s),
            transport=transport,
        )

    def __repr__(self) -> str:
        return f"SuperMarketClient(base_url={self.base_url!r})"

    async def __aenter__(self) -> SuperMarketClient:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        await self._http.aclose()

    # ── transport ───────────────────────────────────────────────────────────────────────────

    def _emit(self, record: HttpRecord) -> None:
        if self._journal is not None:
            self._journal(record)

    async def _request(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, Any] | None = None,
        body: Any = None,
        idempotency_key: str = "",
        ok: Sequence[int] = (200,),
        redact_fields: Sequence[str] = (),
    ) -> tuple[int, Any]:
        """Send one request. Returns ``(status, decoded_body)`` for statuses in ``ok``; raises otherwise."""
        request = self._http.build_request(
            method,
            path.lstrip("/"),
            params=_clean(params or {}),
            content=orjson.dumps(body) if body is not None else None,
            headers={"Content-Type": "application/json"} if body is not None else None,
        )
        journal_path = request.url.raw_path.decode()
        correlation_id = uuid.uuid4().hex
        body_text = request.content.decode() if body is not None else ""
        self._emit(
            HttpRecord(
                ts_ns=time.time_ns(),
                direction="request",
                method=method,
                path=journal_path,
                body_json=body_text,
                status=0,
                latency_us=0,
                request_id="",
                idempotency_key=idempotency_key,
                correlation_id=correlation_id,
            )
        )
        started = time.perf_counter()
        try:
            response = await self._http.send(request)
        except httpx.TransportError as exc:
            latency_us = int((time.perf_counter() - started) * 1e6)
            self._emit(
                HttpRecord(
                    ts_ns=time.time_ns(),
                    direction="error",
                    method=method,
                    path=journal_path,
                    body_json=orjson.dumps({"network_error": type(exc).__name__}).decode(),
                    status=0,
                    latency_us=latency_us,
                    request_id="",
                    idempotency_key=idempotency_key,
                    correlation_id=correlation_id,
                )
            )
            raise NetworkError(f"{method} {journal_path}: {type(exc).__name__}", cause=exc) from exc

        latency_us = int((time.perf_counter() - started) * 1e6)
        request_id = response.headers.get("x-request-id", "")
        text = response.text
        self._emit(
            HttpRecord(
                ts_ns=time.time_ns(),
                direction="response" if response.status_code in ok else "error",
                method=method,
                path=journal_path,
                body_json=_redact(text, redact_fields) if redact_fields else text,
                status=response.status_code,
                latency_us=latency_us,
                request_id=request_id,
                idempotency_key=idempotency_key,
                correlation_id=correlation_id,
            )
        )
        try:
            decoded: Any = orjson.loads(text) if text else None
        except orjson.JSONDecodeError:
            decoded = None

        if response.status_code in ok:
            if decoded is None and text:
                raise ApiError(
                    response.status_code,
                    "INVALID_RESPONSE",
                    "response body is not JSON",
                    request_id=request_id,
                )
            return response.status_code, decoded

        err = decoded.get("error") if isinstance(decoded, dict) else None
        if isinstance(err, dict):
            code = str(err.get("code") or f"HTTP_{response.status_code}")
            message = str(err.get("message") or "")
            details = err.get("details") if isinstance(err.get("details"), dict) else {}
        else:
            code = f"HTTP_{response.status_code}"
            message = text[:200]
            details = {}
        raise ApiError(
            response.status_code,
            code,
            message,
            details,
            retry_after_s=parse_retry_after(response.headers.get("retry-after")),
            request_id=request_id or None,
            body=decoded,
        )

    async def _get(self, path: str, **params: Any) -> Any:
        return (await self._request("GET", path, params=params))[1]

    # ── identity & tournament ───────────────────────────────────────────────────────────────

    async def get_account(self) -> Account:
        return Account.model_validate(await self._get("/account"))

    async def list_tournaments(self, *, status: str = "any", limit: int = 100) -> TournamentsPage:
        return TournamentsPage.model_validate(await self._get("/tournaments", status=status, limit=limit))

    async def get_tournament(self, slug: str) -> Tournament:
        return Tournament.model_validate(await self._get(f"/tournaments/{slug}"))

    # ── markets ─────────────────────────────────────────────────────────────────────────────

    async def list_tournament_markets(
        self,
        slug: str,
        *,
        status: str = "open",
        limit: int = 100,
        cursor: str | None = None,
        search: str | None = None,
    ) -> MarketsPage:
        data = await self._get(
            f"/tournaments/{slug}/markets", status=status, limit=limit, cursor=cursor, search=search
        )
        return MarketsPage.model_validate(data)

    async def iter_tournament_markets(self, slug: str, *, status: str = "open") -> AsyncIterator[Market]:
        cursor: str | None = None
        while True:
            page = await self.list_tournament_markets(slug, status=status, cursor=cursor)
            for market in page.data:
                yield market
            if not page.pagination.has_more or not page.pagination.next_cursor:
                return
            cursor = page.pagination.next_cursor

    async def get_tournament_market(self, slug: str, market_id: str) -> Market:
        """Market detail with the tournament book embedded per exchange."""
        return Market.model_validate(await self._get(f"/tournaments/{slug}/markets/{market_id}"))

    async def get_market_nodes(self, market_id: str, tournament_id: str) -> MarketNodes:
        return MarketNodes.model_validate(
            await self._get(f"/markets/{market_id}/nodes", tournamentId=tournament_id)
        )

    # ── exchanges ───────────────────────────────────────────────────────────────────────────

    async def get_prices(self, exchange_ids: Sequence[str], tournament_id: str) -> BulkPrices:
        """Bulk top-of-book. Splits into ≤100-id requests; preserves request order across chunks."""
        merged = BulkPrices(data=[], missing_ids=[])
        for chunk in _chunks(list(exchange_ids), MAX_PRICE_IDS):
            page = BulkPrices.model_validate(
                await self._get("/exchanges/prices", ids=",".join(chunk), tournamentId=tournament_id)
            )
            merged.data.extend(page.data)
            merged.missing_ids.extend(page.missing_ids)
        return merged

    async def get_orderbook(self, exchange_id: str, tournament_id: str, *, depth: int = 20) -> OrderBook:
        return OrderBook.model_validate(
            await self._get(
                f"/exchanges/{exchange_id}/orderbook", depth=min(depth, 200), tournamentId=tournament_id
            )
        )

    async def get_trades(
        self,
        exchange_id: str,
        tournament_id: str,
        *,
        limit: int = 50,
        cursor: str | None = None,
        from_: str | None = None,
        to: str | None = None,
    ) -> TradesPage:
        return TradesPage.model_validate(
            await self._get(
                f"/exchanges/{exchange_id}/trades",
                tournamentId=tournament_id,
                limit=min(limit, 200),
                cursor=cursor,
                **{"from": from_, "to": to},
            )
        )

    async def get_price_history(
        self,
        exchange_id: str,
        tournament_id: str,
        *,
        resolution: str = "1h",
        from_: str | None = None,
        to: str | None = None,
        limit: int | None = None,
    ) -> PriceHistory:
        return PriceHistory.model_validate(
            await self._get(
                f"/exchanges/{exchange_id}/price-history",
                tournamentId=tournament_id,
                resolution=resolution,
                limit=limit,
                **{"from": from_, "to": to},
            )
        )

    # ── orders (writes) ─────────────────────────────────────────────────────────────────────

    async def place_order(self, order: OrderRequest, idempotency_key: str) -> PlaceOrderResult:
        return await self.place_order_body(order.to_body(), idempotency_key)

    async def place_order_body(self, body: dict[str, Any], idempotency_key: str) -> PlaceOrderResult:
        """Raw placement for probes (e.g. the deliberately off-tick order in ``measure_day1``)."""
        _, data = await self._request(
            "POST",
            "/orders",
            body={**body, "idempotencyKey": idempotency_key},
            idempotency_key=idempotency_key,
        )
        return PlaceOrderResult.model_validate(data)

    async def place_batch(self, orders: Sequence[OrderRequest], idempotency_key: str) -> BatchResult:
        """Best-effort batch (≤50). 200/207/422 return a :class:`BatchResult`; 429/502/503 raise."""
        if not 0 < len(orders) <= MAX_BATCH_ORDERS:
            raise ValueError(f"batch size must be 1..{MAX_BATCH_ORDERS}")
        status, data = await self._request(
            "POST",
            "/orders/batch",
            body={"idempotencyKey": idempotency_key, "orders": [o.to_body() for o in orders]},
            idempotency_key=idempotency_key,
            ok=(200, 207, 422),
        )
        if not isinstance(data, dict) or "results" not in data:
            raise ApiError(status, "INVALID_RESPONSE", "batch response without results", body=data)
        return BatchResult.model_validate({**data, "httpStatus": status})

    async def place_multi_leg(
        self,
        legs: Sequence[OrderRequest],
        idempotency_key: str,
        relationship_constraint: str | None = None,
    ) -> MultiLegResult:
        """Atomic multi-leg (≤10): all legs persist or none do."""
        if not 0 < len(legs) <= MAX_MULTI_LEG:
            raise ValueError(f"multi-leg size must be 1..{MAX_MULTI_LEG}")
        body: dict[str, Any] = {"idempotencyKey": idempotency_key, "legs": [leg.to_body() for leg in legs]}
        if relationship_constraint is not None:
            body["relationshipConstraint"] = relationship_constraint
        _, data = await self._request("POST", "/orders/multi-leg", body=body, idempotency_key=idempotency_key)
        return MultiLegResult.model_validate(data)

    async def cancel_order(self, order_id: int) -> CancelResult:
        """409 CONFLICT (already closed) is raised; callers usually treat it as success."""
        _, data = await self._request("DELETE", f"/orders/{order_id}")
        return CancelResult.model_validate(data)

    async def cancel_all(
        self, tournament_id: str, *, exchange_id: str | None = None, market_id: str | None = None
    ) -> CancelAllResult:
        """200 all cancelled, 207 partial, 422 all failed. 503 (paused) raises with ``details``."""
        if exchange_id is not None and market_id is not None:
            raise ValueError("exchange_id and market_id are mutually exclusive")
        body = _clean({"tournamentId": tournament_id, "exchangeId": exchange_id, "marketId": market_id})
        status, data = await self._request("POST", "/orders/cancel-all", body=body, ok=(200, 207, 422))
        if not isinstance(data, dict) or "cancelled" not in data:
            raise ApiError(status, "INVALID_RESPONSE", "cancel-all response without counts", body=data)
        return CancelAllResult.model_validate({**data, "httpStatus": status})

    # ── orders (reads) ──────────────────────────────────────────────────────────────────────

    async def list_orders(
        self,
        tournament_id: str,
        *,
        status: str = "open",
        exchange_id: str | None = None,
        market_id: str | None = None,
        limit: int = 200,
        cursor: str | None = None,
    ) -> OrdersPage:
        return OrdersPage.model_validate(
            await self._get(
                "/orders",
                tournamentId=tournament_id,
                status=status,
                exchangeId=exchange_id,
                marketId=market_id,
                limit=min(limit, 200),
                cursor=cursor,
            )
        )

    async def iter_orders(
        self,
        tournament_id: str,
        *,
        status: str = "open",
        exchange_id: str | None = None,
        market_id: str | None = None,
    ) -> AsyncIterator[Order]:
        cursor: str | None = None
        while True:
            page = await self.list_orders(
                tournament_id, status=status, exchange_id=exchange_id, market_id=market_id, cursor=cursor
            )
            for order in page.data:
                yield order
            if not page.pagination.has_more or not page.pagination.next_cursor:
                return
            cursor = page.pagination.next_cursor

    async def list_open_orders(
        self, tournament_id: str, *, exchange_id: str | None = None, market_id: str | None = None
    ) -> list[Order]:
        return [
            o
            async for o in self.iter_orders(
                tournament_id, status="open", exchange_id=exchange_id, market_id=market_id
            )
        ]

    async def get_order(self, order_id: int) -> Order:
        return Order.model_validate(await self._get(f"/orders/{order_id}"))

    async def get_order_fills(self, order_id: int, *, limit: int = 200) -> OrderFills:
        return OrderFills.model_validate(await self._get(f"/orders/{order_id}/fills", limit=limit))

    # ── portfolio (tournament-scoped forms) ─────────────────────────────────────────────────

    async def get_positions(self, slug: str) -> Positions:
        return Positions.model_validate(await self._get(f"/tournaments/{slug}/portfolio/positions"))

    async def get_pnl(self, slug: str, *, period: str = "all") -> PnL:
        return PnL.model_validate(await self._get(f"/tournaments/{slug}/portfolio/pnl", period=period))

    async def get_fills(
        self,
        slug: str,
        *,
        exchange_id: str | None = None,
        market_id: str | None = None,
        limit: int = 200,
        cursor: str | None = None,
    ) -> FillsPage:
        return FillsPage.model_validate(
            await self._get(
                f"/tournaments/{slug}/portfolio/fills",
                exchangeId=exchange_id,
                marketId=market_id,
                limit=min(limit, 200),
                cursor=cursor,
            )
        )

    async def iter_fills(self, slug: str, *, exchange_id: str | None = None) -> AsyncIterator[Fill]:
        cursor: str | None = None
        while True:
            page = await self.get_fills(slug, exchange_id=exchange_id, cursor=cursor)
            for fill in page.data:
                yield fill
            if not page.pagination.has_more or not page.pagination.next_cursor:
                return
            cursor = page.pagination.next_cursor

    async def get_settlements(
        self, slug: str, *, limit: int = 200, cursor: str | None = None
    ) -> SettlementsPage:
        return SettlementsPage.model_validate(
            await self._get(f"/tournaments/{slug}/portfolio/settlements", limit=limit, cursor=cursor)
        )

    async def get_collateral(self, tournament_id: str) -> Collateral:
        return Collateral.model_validate(await self._get("/portfolio/collateral", tournamentId=tournament_id))

    async def get_transactions(
        self, *, types: str | None = None, limit: int = 200, cursor: str | None = None
    ) -> TransactionsPage:
        """``GET /portfolio/transactions`` (the key's default tournament): the only read that
        documents ALL collateral detail (``outstandingAdvanceAfter``, ``componentId``, ``reason``)."""
        return TransactionsPage.model_validate(
            await self._get("/portfolio/transactions", type=types, limit=limit, cursor=cursor)
        )

    async def get_leaderboard(
        self, slug: str, *, period: str = "all", limit: int = 100, offset: int = 0
    ) -> Leaderboard:
        return Leaderboard.model_validate(
            await self._get(
                f"/tournaments/{slug}/leaderboard", period=period, limit=min(limit, 100), offset=offset
            )
        )

    # ── relationships ───────────────────────────────────────────────────────────────────────

    async def list_relationships(
        self,
        tournament_id: str,
        *,
        type_: str | None = None,
        market_id: str | None = None,
        exchange_id: str | None = None,
        limit: int = 200,
        cursor: str | None = None,
    ) -> RelationshipsPage:
        return RelationshipsPage.model_validate(
            await self._get(
                "/relationships",
                tournamentId=tournament_id,
                type=type_,
                marketId=market_id,
                exchangeId=exchange_id,
                limit=min(limit, 200),
                cursor=cursor,
            )
        )

    async def iter_relationships(
        self, tournament_id: str, *, type_: str | None = None
    ) -> AsyncIterator[Relationship]:
        cursor: str | None = None
        while True:
            page = await self.list_relationships(tournament_id, type_=type_, cursor=cursor)
            for rel in page.data:
                yield rel
            if not page.pagination.has_more or not page.pagination.next_cursor:
                return
            cursor = page.pagination.next_cursor

    async def get_relationship_graph(self, market_id: str, tournament_id: str, *, depth: int = 2) -> Any:
        return await self._get(
            "/relationships/graph", marketId=market_id, tournamentId=tournament_id, depth=depth
        )

    async def get_constraints(
        self,
        tournament_id: str,
        *,
        violations_only: bool = True,
        min_violation: float | None = 0.01,
        market_id: str | None = None,
        relationship_id: str | None = None,
    ) -> ConstraintsResult:
        return ConstraintsResult.model_validate(
            await self._get(
                "/relationships/constraints",
                tournamentId=tournament_id,
                violationsOnly=violations_only,
                minViolation=min_violation,
                marketId=market_id,
                relationshipId=relationship_id,
            )
        )

    # ── realtime ────────────────────────────────────────────────────────────────────────────

    async def mint_realtime_token(self) -> RealtimeToken:
        _, data = await self._request("POST", "/realtime/token", redact_fields=("token", "anonKey"))
        return RealtimeToken.model_validate(data)
