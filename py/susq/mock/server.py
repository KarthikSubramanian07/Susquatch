"""The mock Super Market: a Starlette app serving ``/api/v1``, a Phoenix websocket and ``/_mock`` admin.

Run it with ``uv run susq-mock --port 8787`` (see ``py/susq/mock/README.md``), or embed it in tests:

    app = create_app(MockConfig(seed=7))
    transport = httpx.ASGITransport(app=app)
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import binascii
import random
import secrets
import uuid
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from typing import Any

import orjson
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Route, WebSocketRoute
from starlette.websockets import WebSocket

from susq.mock.engine import (
    Engine,
    EngineError,
    FillRecord,
    Placement,
    RestingOrder,
    SettlementRecord,
    TradeRecord,
    iso,
    parse_iso,
)
from susq.mock.faults import Clock, Faults, RateWindow
from susq.mock.realtime import PhoenixHub, TokenInfo
from susq.mock.scenario import Scenario, ScenarioConfig
from susq.platform.prices import PIPS_PER_UNIT, to_pips, to_price

__all__ = ["MockConfig", "MockExchange", "config_from_args", "create_app", "main"]

API = "/api/v1"
REALTIME_TTL = timedelta(hours=3)
_RESOLUTIONS = {"1m": 60, "5m": 300, "1h": 3600, "1d": 86400, "1w": 604800}


@dataclass
class MockConfig:
    seed: int = 7
    tournament_slug: str = "midterm-elections"
    api_keys: dict[str, str] = field(default_factory=lambda: {"mock-key-alpha": "trader-alpha"})
    sponsor_half_width_pips: int = 100
    sponsor_size: int = 500
    synthetic_players: int = 40
    #: Model ALL collateral (relationship-backed advances); False = plain cash, ``all: null``.
    collateral: bool = True
    faults: Faults = field(default_factory=Faults)
    frozen_now: datetime | None = None
    #: Public origin for ``supabaseUrl`` in realtime tokens (the websocket lives on this same app).
    public_origin: str = "http://127.0.0.1:8787"


def _json(data: Any, status: int = 200, headers: dict[str, str] | None = None) -> Response:
    return Response(orjson.dumps(data), status_code=status, media_type="application/json", headers=headers)


def _encode_cursor(offset: int) -> str:
    return base64.urlsafe_b64encode(f"o:{offset}".encode()).decode()


def _decode_cursor(cursor: str | None) -> int:
    if not cursor:
        return 0
    try:
        raw = base64.urlsafe_b64decode(cursor.encode()).decode()
        if not raw.startswith("o:"):
            raise ValueError(raw)
        return int(raw[2:])
    except (ValueError, binascii.Error, UnicodeDecodeError) as exc:
        raise EngineError(400, "INVALID_CURSOR", "Pagination cursor could not be decoded.") from exc


def _int_param(request: Request, name: str, default: int, maximum: int) -> int:
    raw = request.query_params.get(name)
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError as exc:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.", {name: ["integer"]}) from exc
    if not 1 <= value <= maximum:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.", {name: [f"1..{maximum}"]})
    return value


def _page(
    items: list[Any], request: Request, *, default: int, maximum: int
) -> tuple[list[Any], dict[str, Any]]:
    limit = _int_param(request, "limit", default, maximum)
    offset = _decode_cursor(request.query_params.get("cursor"))
    chunk = items[offset : offset + limit]
    has_more = offset + limit < len(items)
    return chunk, {
        "total": len(items),
        "limit": limit,
        "hasMore": has_more,
        "nextCursor": _encode_cursor(offset + limit) if has_more else None,
    }


class MockExchange:
    """All mock state: scenario/engine, idempotency store, realtime tokens, faults."""

    def __init__(self, config: MockConfig) -> None:
        self.config = config
        self.clock = Clock(config.frozen_now)
        self.faults = config.faults
        self.rate = RateWindow(lambda: self.clock.now().timestamp())
        self.hub = PhoenixHub(self._resolve_token, self.clock.now)
        self.reset(config.seed)

    def reset(self, seed: int) -> None:
        self.config.seed = seed
        self.scenario = Scenario(
            ScenarioConfig(
                seed=seed,
                tournament_slug=self.config.tournament_slug,
                sponsor_half_width_pips=self.config.sponsor_half_width_pips,
                sponsor_size=self.config.sponsor_size,
                synthetic_players=self.config.synthetic_players,
                collateral=self.config.collateral,
                api_keys=dict(self.config.api_keys),
            ),
            self.clock.now(),
        )
        self.fault_rng = random.Random(seed + 1)  # noqa: S311 - simulation, not crypto
        #: (profile, endpoint, key) -> (fingerprint, status, body)
        self.idempotency: dict[tuple[str, str, str], tuple[bytes, int, Any]] = {}
        self.tokens: dict[str, TokenInfo] = {}
        #: profile -> collateral events already broadcast on its ``user:`` channel
        self.collateral_published: dict[str, int] = {}
        self.rate.reset()

    @property
    def engine(self) -> Engine:
        return self.scenario.engine

    @property
    def tid(self) -> str:
        return self.scenario.tournament_id

    def _resolve_token(self, token: str) -> TokenInfo | None:
        return self.tokens.get(token)

    # ── request gate ────────────────────────────────────────────────────────────────────────

    async def gate(self, request: Request) -> str:
        """Latency, random 503, auth, rate limit. Returns the caller's profile id."""
        if self.faults.latency_ms:
            await asyncio.sleep(self.faults.latency_ms / 1000)
        auth = request.headers.get("authorization", "")
        key = auth[7:].strip() if auth.lower().startswith("bearer ") else request.headers.get("x-api-key", "")
        if not key:
            raise EngineError(401, "MISSING_API_KEY", "No API key provided.")
        profile = self.scenario.keys.get(key)
        if profile is None:
            raise EngineError(401, "INVALID_API_KEY", "API key not recognised.")
        if self.faults.rate_limit_rps is not None and not self.rate.allow(key, self.faults.rate_limit_rps):
            raise EngineError(
                429,
                "RATE_LIMITED",
                "Rate limit exceeded.",
                headers={"Retry-After": str(self.faults.retry_after_s)},
            )
        if self.faults.random_503_prob and self.fault_rng.random() < self.faults.random_503_prob:
            raise EngineError(503, "SERVICE_UNAVAILABLE", "Service temporarily unavailable.")
        return profile

    def require_tournament(self, request: Request) -> None:
        tid = request.query_params.get("tournamentId")
        if tid is None:
            raise EngineError(
                400,
                "VALIDATION_ERROR",
                "Invalid query parameters.",
                {"tournamentId": ["The mock requires an explicit tournamentId."]},
            )
        if tid != self.tid:
            raise EngineError(403, "FORBIDDEN", "Not a member of this tournament.")

    def require_slug(self, request: Request) -> None:
        if request.path_params["slug"] != self.config.tournament_slug:
            raise EngineError(404, "NOT_FOUND", "Tournament not found.")

    # ── idempotent writes ───────────────────────────────────────────────────────────────────

    async def idempotent(
        self,
        profile: str,
        endpoint: str,
        body: Any,
        run: Callable[[], Awaitable[tuple[int, Any]]],
    ) -> tuple[int, Any]:
        if not isinstance(body, dict):
            raise EngineError(400, "INVALID_JSON", "Request body is not valid JSON.")
        key = body.get("idempotencyKey")
        if not isinstance(key, str) or not 1 <= len(key) <= 255:
            raise EngineError(
                400, "VALIDATION_ERROR", "Invalid request body.", {"idempotencyKey": ["required"]}
            )
        fingerprint = orjson.dumps(
            {k: v for k, v in body.items() if k != "idempotencyKey"}, option=orjson.OPT_SORT_KEYS
        )
        slot = (profile, endpoint, key)
        stored = self.idempotency.get(slot)
        if stored is not None:
            if stored[0] != fingerprint:
                raise EngineError(409, "CONFLICT", "Idempotency key reused with a different payload.")
            return stored[1], stored[2]
        if self.faults.take("request_in_flight_next"):
            raise EngineError(409, "REQUEST_IN_FLIGHT", "An identical request is still executing.")
        if self.faults.take("service_unavailable_next"):
            raise EngineError(503, "SERVICE_UNAVAILABLE", "The trading engine could not confirm the order.")
        status, result = await run()
        if status < 500:
            self.idempotency[slot] = (fingerprint, status, result)
        if self.faults.take("order_status_unknown_next"):
            raise EngineError(
                502,
                "ORDER_STATUS_UNKNOWN",
                "The trading engine did not confirm this order in time. It may still have gone through.",
            )
        return status, result

    # ── realtime fan-out ────────────────────────────────────────────────────────────────────

    async def publish(
        self, trades: list[TradeRecord], touched: set[str], new_fills: list[FillRecord]
    ) -> None:
        now = self.clock.now()
        eng = self.engine
        # The Sponsor replenishes any quote it was filled on.
        sponsor = self.scenario.sponsor_id
        for ex in sorted({t.exchange_id for t in trades if sponsor in (t.buyer, t.seller)}):
            self.scenario.requote(ex, now)
            touched.add(ex)
        if not touched and not trades:
            return
        await self.hub.broadcast(
            f"tournament:{self.tid}",
            "market_batch",
            {
                "trades": [
                    {
                        "exchangeId": t.exchange_id,
                        "marketId": t.market_id,
                        "price": to_price(t.yes_price),
                        "quantity": t.size,
                        "executedAt": iso(t.created_at),
                        "tournamentId": self.tid,
                    }
                    for t in trades
                ],
                "bookDirty": [
                    {
                        "exchangeId": ex,
                        "marketId": eng.by_exchange[ex].market_id,
                        "tournamentId": self.tid,
                        "at": iso(now),
                    }
                    for ex in sorted(touched)
                ],
                "marketSettled": [],
            },
        )
        by_owner: dict[str, list[FillRecord]] = {}
        for f in new_fills:
            by_owner.setdefault(f.owner, []).append(f)
        for owner in self.pending_collateral():
            by_owner.setdefault(owner, [])
        for owner, fills in by_owner.items():
            await self.hub.broadcast(
                f"user:{owner}",
                "account_batch",
                {
                    "fills": [
                        {
                            "orderId": f.order_id,
                            "exchangeId": f.exchange_id,
                            "marketId": f.market_id,
                            "price": to_price(f.yes_price),
                            "quantity": f.quantity,
                            "executedAt": iso(f.filled_at),
                            "tournamentId": self.tid,
                        }
                        for f in fills
                    ],
                    "orderUpdates": [
                        self._order_update(eng.state.orders[oid], now)
                        for oid in sorted({f.order_id for f in fills})
                    ],
                    "settlements": [],
                    "refunds": [],
                    "collateralChanges": self.take_collateral_changes(owner),
                },
            )
        if trades:
            await self.broadcast_violations({t.exchange_id for t in trades})

    def pending_collateral(self) -> list[str]:
        """Profiles with collateral events not yet broadcast (bots excluded)."""
        return [
            pid
            for pid, acct in self.engine.state.accounts.items()
            if not acct.is_bot and len(acct.collateral.events) > self.collateral_published.get(pid, 0)
        ]

    def take_collateral_changes(self, profile: str) -> list[dict[str, Any]]:
        """``account_batch.collateralChanges`` items since the last broadcast to ``profile``."""
        events = self.engine.account(profile).collateral.events
        start = min(self.collateral_published.get(profile, 0), len(events))
        self.collateral_published[profile] = len(events)
        return [
            {
                "delta": e.delta_pips / PIPS_PER_UNIT,
                "outstandingAdvanceAfter": e.outstanding_after_pips / PIPS_PER_UNIT,
                "componentId": e.component_id,
                "tournamentId": self.tid,
            }
            for e in events[start:]
        ]

    def _order_update(self, order: RestingOrder, now: datetime) -> dict[str, Any]:
        return {
            "orderId": order.id,
            "exchangeId": order.exchange_id,
            "marketId": self.engine.by_exchange[order.exchange_id].market_id,
            "open": order.status == "open",
            "quantityTraded": order.quantity - order.remaining,
            "totalCost": None,
            "latestTradePrice": to_price(self.engine.mark(order.exchange_id) or 0),
            "tournamentId": self.tid,
            "at": iso(now),
        }

    async def broadcast_violations(self, exchanges: set[str]) -> None:
        for rel in self.scenario.relationships:
            if not exchanges & set(rel.exchange_ids):
                continue
            ev = self.scenario.evaluate(rel)
            if ev["evaluationStatus"] != "violated":
                continue
            await self.hub.broadcast(
                f"relationships:violations:{self.tid}",
                "violation",
                {
                    **ev,
                    "computedAt": iso(self.clock.now()),
                    "delivery": {
                        "model": "best-effort-authoritative-resync",
                        "correlationId": uuid.uuid4().hex,
                        "sourceSequence": len(self.engine.state.trades),
                    },
                },
            )

    # ── serializers ─────────────────────────────────────────────────────────────────────────

    def order_json(self, o: RestingOrder) -> dict[str, Any]:
        return {
            "id": o.id,
            "exchangeId": o.exchange_id,
            "side": o.side,
            "action": o.action,
            "quantity": o.remaining,
            "priceLimit": to_price(o.price_pips) if o.price_pips else None,
            "open": o.status == "open",
            "createdAt": iso(o.created_at),
            "expirationDate": iso(o.expiration) if o.expiration else None,
        }

    def market_json(self, market_id: str, *, with_book: bool = False) -> dict[str, Any]:
        m = self.engine.markets[market_id]
        mark = self.engine.mark(m.exchange_id)
        exchange: dict[str, Any] = {
            "id": m.exchange_id,
            "option": m.option,
            "latestPrice": to_price(mark) if mark is not None else None,
            "initialPrice": m.initial_price,
        }
        out: dict[str, Any] = {
            "id": m.market_id,
            "title": m.title,
            "status": m.status,
            "createdAt": "2026-09-20T00:00:00Z",
            "settlementDate": m.settlement_date,
            "settledWith": m.settled_with,
            "settledOn": iso(m.settled_on) if m.settled_on else None,
            "categories": [m.category],
            "isComposite": False,
            "isMultiOutcome": False,
            "exchanges": [exchange],
            "creator": None,
        }
        if with_book:
            book = self.engine.book(m.exchange_id, self.clock.now())
            exchange["bids"] = [{"price": to_price(p), "quantity": q} for p, q in book["bids"]]
            exchange["asks"] = [{"price": to_price(p), "quantity": q} for p, q in book["asks"]]
            exchange["bestBid"] = exchange["bids"][0]["price"] if book["bids"] else None
            exchange["bestAsk"] = exchange["asks"][0]["price"] if book["asks"] else None
            implied = exchange["bestAsk"] if exchange["bestAsk"] is not None else exchange["latestPrice"]
            out["overround"] = implied
            out["hasArbitrageOpportunity"] = False
        return out

    def price_json(self, ex: str) -> dict[str, Any]:
        bid, ask = self.engine.best(ex, self.clock.now())
        mark = self.engine.mark(ex)
        m = self.engine.by_exchange[ex]
        return {
            "exchangeId": ex,
            "marketId": m.market_id,
            "option": m.option,
            "latestPrice": to_price(mark) if mark is not None else None,
            "bestBid": to_price(bid) if bid is not None else None,
            "bestAsk": to_price(ask) if ask is not None else None,
            "spread": to_price(ask - bid) if bid is not None and ask is not None else None,
        }


# ── handlers ────────────────────────────────────────────────────────────────────────────────

Handler = Callable[[MockExchange, Request, str], Awaitable[Response]]


def api(handler: Handler) -> Callable[[Request], Awaitable[Response]]:
    async def endpoint(request: Request) -> Response:
        mx: MockExchange = request.app.state.mx
        rid = str(uuid.uuid4())
        try:
            profile = await mx.gate(request)
            response = await handler(mx, request, profile)
        except EngineError as err:
            response = _json(err.envelope(), err.status, err.headers)
        response.headers["x-request-id"] = rid
        return response

    return endpoint


async def _body(request: Request) -> Any:
    raw = await request.body()
    if not raw:
        return {}
    try:
        return orjson.loads(raw)
    except orjson.JSONDecodeError as exc:
        raise EngineError(400, "INVALID_JSON", "Request body is not valid JSON.") from exc


async def get_account(mx: MockExchange, request: Request, profile: str) -> Response:
    acct = mx.engine.account(profile)
    return _json(
        {
            "id": profile,
            "username": acct.username,
            "email": None,
            "createdAt": "2026-09-20T00:00:00Z",
            "avatarUrl": None,
            "bio": None,
            "balance": acct.ledger.cash,
        }
    )


def _tournament_json(mx: MockExchange, profile: str) -> dict[str, Any]:
    acct = mx.engine.account(profile)
    return {
        "id": mx.tid,
        "slug": mx.config.tournament_slug,
        "name": "SIG Predictions Cup: Midterm Elections (mock)",
        "description": "Synthetic tournament served by susq-mock.",
        "status": "active",
        "startDate": "2026-10-01T16:00:00Z",
        "endDate": "2026-11-04T17:00:00Z",
        "initialBalance": 100_000,
        "currencyName": "SUSQies",
        "memberCount": mx.scenario.member_count(),
        "marketCount": len(mx.engine.markets),
        "myBalance": acct.ledger.cash,
        "isPendingEnrolment": False,
        "joinedAt": "2026-09-28T00:00:00Z",
    }


async def list_tournaments(mx: MockExchange, request: Request, profile: str) -> Response:
    return _json(
        {
            "data": [_tournament_json(mx, profile)],
            "pagination": {"limit": 50, "offset": 0, "hasMore": False, "total": 1},
        }
    )


async def get_tournament(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    return _json(_tournament_json(mx, profile))


async def list_markets(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    status = request.query_params.get("status", "any")
    search = (request.query_params.get("search") or "").lower()
    ids = [
        mid
        for mid, m in sorted(mx.engine.markets.items(), key=lambda kv: int(kv[0]))
        if (status in ("any", m.status) or (status == "resolved" and m.status == "settled"))
        and search in m.title.lower()
    ]
    chunk, pagination = _page(ids, request, default=20, maximum=100)
    return _json({"data": [mx.market_json(mid) for mid in chunk], "pagination": pagination})


async def get_market(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    mid = request.path_params["id"]
    if mid not in mx.engine.markets:
        raise EngineError(404, "NOT_FOUND", "Market not found.")
    return _json(mx.market_json(mid, with_book=True))


async def get_nodes(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    mid = request.path_params["id"]
    m = mx.engine.markets.get(mid)
    if m is None:
        raise EngineError(404, "NOT_FOUND", "Market not found.")
    return _json(
        {
            "market_id": mid,
            "root": {
                "nodeType": "contract",
                "position": 0,
                "title": m.title,
                "contractType": "Election Outcome",
                "contractDetails": m.contract_details,
                "settlementOptions": ["YES"],
                "settlementDate": m.settlement_date,
                "settledWith": None,
                "settledOn": None,
            },
            "contexts": [
                {
                    "type": "tournament",
                    "tournament": {
                        "id": mx.tid,
                        "slug": mx.config.tournament_slug,
                        "name": "SIG Predictions Cup (mock)",
                        "currencyName": "SUSQies",
                        "isOngoingPlay": False,
                    },
                }
            ],
        }
    )


def _exchange(mx: MockExchange, request: Request) -> str:
    ex = request.path_params["id"]
    if ex not in mx.engine.by_exchange:
        raise EngineError(404, "NOT_FOUND", "Exchange not found.")
    return str(ex)


async def get_prices(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    raw = request.query_params.get("ids")
    ids = [i for i in (raw or "").split(",") if i]
    if not ids or len(ids) > 100:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.", {"ids": ["1..100 ids"]})
    data = [mx.price_json(i) for i in ids if i in mx.engine.by_exchange]
    missing = [i for i in ids if i not in mx.engine.by_exchange]
    return _json({"data": data, "missingIds": missing})


async def get_price(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    return _json(mx.price_json(_exchange(mx, request)))


async def get_orderbook(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    ex = _exchange(mx, request)
    depth = _int_param(request, "depth", 20, 200)
    book = mx.engine.book(ex, mx.clock.now(), depth=depth)
    price = mx.price_json(ex)
    return _json(
        {
            "exchangeId": ex,
            "marketId": price["marketId"],
            "depth": depth,
            "bids": [{"price": to_price(p), "quantity": q} for p, q in book["bids"]],
            "asks": [{"price": to_price(p), "quantity": q} for p, q in book["asks"]],
            "bestBid": price["bestBid"],
            "bestAsk": price["bestAsk"],
            "spread": price["spread"],
        }
    )


async def get_trades(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    ex = _exchange(mx, request)
    trades = [t for t in reversed(mx.engine.state.trades) if t.exchange_id == ex]
    chunk, pagination = _page(trades, request, default=50, maximum=200)
    pagination.pop("total")
    now = mx.clock.now()
    return _json(
        {
            "exchangeId": ex,
            "marketId": mx.engine.by_exchange[ex].market_id,
            "from": "1970-01-01T00:00:00Z",
            "to": iso(now),
            "data": [
                {
                    "id": str(t.id),
                    "createdAt": iso(t.created_at),
                    "price": to_price(t.yes_price),
                    "size": t.size,
                    "side": t.taker_side,
                    "volume": round(t.size * t.yes_price / PIPS_PER_UNIT, 4),
                }
                for t in chunk
            ],
            "pagination": pagination,
            "coverage": {"complete": True, "projectedThroughSequence": len(mx.engine.state.trades)},
        }
    )


async def get_price_history(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    ex = _exchange(mx, request)
    resolution = request.query_params.get("resolution", "1h")
    if resolution not in _RESOLUTIONS:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.", {"resolution": ["enum"]})
    step = _RESOLUTIONS[resolution]
    buckets: dict[int, list[TradeRecord]] = {}
    for t in mx.engine.state.trades:
        if t.exchange_id == ex:
            buckets.setdefault(int(t.created_at.timestamp()) // step * step, []).append(t)
    candles = []
    for start in sorted(buckets):
        ts = buckets[start]
        prices = [t.yes_price for t in ts]
        volume = sum(t.size for t in ts)
        candles.append(
            {
                "time": iso(datetime.fromtimestamp(start, UTC)),
                "open": to_price(prices[0]),
                "high": to_price(max(prices)),
                "low": to_price(min(prices)),
                "close": to_price(prices[-1]),
                "vwap": round(sum(t.yes_price * t.size for t in ts) / volume / PIPS_PER_UNIT, 6),
                "volume": volume,
                "tradeCount": len(ts),
            }
        )
    limit = _int_param(request, "limit", 200, 1000)
    now = mx.clock.now()
    return _json(
        {
            "exchangeId": ex,
            "marketId": mx.engine.by_exchange[ex].market_id,
            "resolution": resolution,
            "from": candles[-limit:][0]["time"] if candles else iso(now),
            "to": iso(now),
            "candles": candles[-limit:],
            "coverage": {"complete": True, "projectedThroughSequence": len(mx.engine.state.trades)},
        }
    )


# ── orders ──────────────────────────────────────────────────────────────────────────────────


async def post_order(mx: MockExchange, request: Request, profile: str) -> Response:
    body = await _body(request)
    new_fills: list[FillRecord] = []
    placements: list[Placement] = []

    async def run() -> tuple[int, Any]:
        before = len(mx.engine.state.fills)
        order = {k: v for k, v in body.items() if k != "idempotencyKey"}
        placement = mx.engine.place(profile, order, mx.clock.now())
        new_fills.extend(mx.engine.state.fills[before:])
        placements.append(placement)
        return 200, placement.body

    try:
        status, data = await mx.idempotent(profile, "orders", body, run)
    finally:
        for p in placements:
            await mx.publish(p.trades, set(p.touched), new_fills)
    return _json(data, status)


def _item_error(err: EngineError, index: int) -> dict[str, Any]:
    return {"index": index, "ok": False, "status": err.status, "data": err.envelope()}


async def post_batch(mx: MockExchange, request: Request, profile: str) -> Response:
    body = await _body(request)
    orders = body.get("orders") if isinstance(body, dict) else None
    if not isinstance(orders, list) or not 1 <= len(orders) <= 50:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid request body.", {"orders": ["1..50 orders"]})
    now = mx.clock.now()
    for i, order in enumerate(orders):  # all-or-nothing validation before any placement
        try:
            mx.engine.validate(order, now)
        except EngineError as err:
            details = {"index": [str(i)], **err.details}
            raise EngineError(400, "VALIDATION_ERROR", "Invalid request body.", details) from err
    new_fills: list[FillRecord] = []
    placements: list[Placement] = []

    async def run() -> tuple[int, Any]:
        results: list[dict[str, Any]] = []
        for i, order in enumerate(orders):
            before = len(mx.engine.state.fills)
            try:
                placement = mx.engine.place(profile, order, mx.clock.now())
            except EngineError as err:
                results.append(_item_error(err, i))
                continue
            new_fills.extend(mx.engine.state.fills[before:])
            placements.append(placement)
            results.append({"index": i, "ok": True, "status": 200, "data": placement.body})
        oks = sum(1 for r in results if r["ok"])
        status = 200 if oks == len(results) else (207 if oks else 422)
        return status, {"results": results}

    try:
        status, data = await mx.idempotent(profile, "orders/batch", body, run)
    finally:
        trades = [t for p in placements for t in p.trades]
        touched = {ex for p in placements for ex in p.touched}
        if placements:
            await mx.publish(trades, touched, new_fills)
    return _json(data, status)


async def post_multi_leg(mx: MockExchange, request: Request, profile: str) -> Response:
    body = await _body(request)
    legs = body.get("legs") if isinstance(body, dict) else None
    if not isinstance(legs, list) or not 1 <= len(legs) <= 10:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid request body.", {"legs": ["1..10 legs"]})
    now = mx.clock.now()
    seen: set[str] = set()
    for i, leg in enumerate(legs):
        try:
            mx.engine.validate(leg, now)
        except EngineError as err:
            raise EngineError(err.status, err.code, err.message, {**err.details, "leg": i}) from err
        if leg["exchangeId"] in seen:
            raise EngineError(400, "VALIDATION_ERROR", "Duplicate legs for the same exchange.", {"leg": i})
        seen.add(leg["exchangeId"])
    constraint = body.get("relationshipConstraint")
    if constraint is not None:
        _check_relationship_constraint(mx, str(constraint), legs)
    new_fills: list[FillRecord] = []
    placements: list[Placement] = []

    async def run() -> tuple[int, Any]:
        snapshot = mx.engine.snapshot()
        results = []
        before = len(mx.engine.state.fills)
        for i, leg in enumerate(legs):
            try:
                placement = mx.engine.place(profile, leg, mx.clock.now())
            except EngineError as err:
                mx.engine.restore(snapshot)
                placements.clear()
                raise EngineError(err.status, err.code, err.message, {**err.details, "leg": i}) from err
            placements.append(placement)
            results.append({"index": i, "data": placement.body})
        new_fills.extend(mx.engine.state.fills[before:])
        return 200, {"results": results}

    try:
        status, data = await mx.idempotent(profile, "orders/multi-leg", body, run)
    finally:
        trades = [t for p in placements for t in p.trades]
        touched = {ex for p in placements for ex in p.touched}
        if placements:
            await mx.publish(trades, touched, new_fills)
    return _json(data, status)


def _check_relationship_constraint(mx: MockExchange, rel_id: str, legs: list[dict[str, Any]]) -> None:
    rel = mx.scenario.relationship(rel_id)
    if rel is None:
        raise EngineError(422, "RELATIONSHIP_VIOLATION", "Unknown or inactive relationship.")
    members = set(rel.exchange_ids)
    yes_prices: dict[str, float] = {}
    for leg in legs:
        if leg["exchangeId"] not in members:
            raise EngineError(422, "RELATIONSHIP_VIOLATION", "Leg exchange is outside the relationship.")
        price = leg.get("price")
        if price is None:
            continue
        yes_prices[leg["exchangeId"]] = price if leg["side"] == "yes" else 1 - price
    for ex in members - yes_prices.keys():
        mark = mx.engine.mark(ex)
        if mark is not None:
            yes_prices[ex] = mark / PIPS_PER_UNIT
    if rel.type == "mutually_exclusive":
        total = sum(yes_prices.values())
        ok = abs(total - 1) < 1e-9 if rel.is_exhaustive else total <= 1 + 1e-9
    else:
        ante, cons = rel.exchange_ids
        ok = yes_prices.get(ante, 0) <= yes_prices.get(cons, 1) + 1e-9
    if not ok:
        raise EngineError(422, "RELATIONSHIP_VIOLATION", "Leg prices violate the relationship rule.")


async def list_orders(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    status = request.query_params.get("status", "open")
    if status not in ("open", "closed", "expired", "all"):
        raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.", {"status": ["enum"]})
    exchange_id = request.query_params.get("exchangeId")
    market_id = request.query_params.get("marketId")
    mx.engine.sweep_expired(mx.clock.now())
    wanted = {
        "open": {"open"},
        "closed": {"filled", "cancelled", "expired"},
        "expired": {"expired"},
        "all": {"open", "filled", "cancelled", "expired"},
    }[status]
    orders = [
        o
        for o in sorted(mx.engine.state.orders.values(), key=lambda o: -o.id)
        if o.owner == profile
        and o.status in wanted
        and (exchange_id is None or o.exchange_id == exchange_id)
        and (market_id is None or mx.engine.by_exchange[o.exchange_id].market_id == market_id)
    ]
    chunk, pagination = _page(orders, request, default=50, maximum=200)
    pagination.pop("total")
    return _json(
        {
            "data": [mx.order_json(o) for o in chunk],
            "pagination": pagination,
            "coverage": {"complete": True, "projectedThroughSequence": len(mx.engine.state.trades)},
        }
    )


def _own_order(mx: MockExchange, request: Request, profile: str) -> RestingOrder:
    mx.engine.sweep_expired(mx.clock.now())
    order = mx.engine.state.orders.get(request.path_params["id"])
    if order is None:
        raise EngineError(404, "NOT_FOUND", "Order not found.")
    if order.owner != profile:
        raise EngineError(403, "FORBIDDEN", "Order belongs to another user.")
    return order


async def get_order(mx: MockExchange, request: Request, profile: str) -> Response:
    return _json(mx.order_json(_own_order(mx, request, profile)))


async def get_order_fills(mx: MockExchange, request: Request, profile: str) -> Response:
    order = _own_order(mx, request, profile)
    fills = [f for f in reversed(mx.engine.state.fills) if f.order_id == order.id and f.owner == profile]
    chunk, pagination = _page(fills, request, default=50, maximum=200)
    pagination.pop("total")
    total = sum(f.quantity for f in fills)
    shares = sum(abs(f.quantity) for f in fills)
    avg = sum(abs(f.quantity) * f.yes_price for f in fills) / shares if shares else None
    return _json(
        {
            "orderId": order.id,
            "exchangeId": order.exchange_id,
            "tournamentId": mx.tid,
            "data": [
                {
                    "id": f.id,
                    "price": to_price(f.yes_price),
                    "quantity": f.quantity,
                    "side": f.side,
                    "filledAt": iso(f.filled_at),
                }
                for f in chunk
            ],
            "pagination": pagination,
            "coverage": {"complete": True, "projectedThroughSequence": len(mx.engine.state.trades)},
            "totalQuantityFilled": total,
            "avgFillPrice": round(avg / PIPS_PER_UNIT, 6) if avg is not None else None,
        }
    )


async def delete_order(mx: MockExchange, request: Request, profile: str) -> Response:
    if mx.faults.take("cancel_unavailable_next"):
        raise EngineError(503, "SERVICE_UNAVAILABLE", "Cancellation was not confirmed.")
    order = mx.engine.cancel(profile, request.path_params["id"], mx.clock.now())
    await mx.publish([], {order.exchange_id}, [])
    return _json({"orderId": order.id, "tournamentId": mx.tid, "message": "Order cancelled."})


async def cancel_all(mx: MockExchange, request: Request, profile: str) -> Response:
    body = await _body(request)
    if not isinstance(body, dict):
        raise EngineError(400, "INVALID_JSON", "Request body is not valid JSON.")
    exchange_id = body.get("exchangeId")
    market_id = body.get("marketId")
    if exchange_id is not None and market_id is not None:
        raise EngineError(400, "VALIDATION_ERROR", "exchangeId and marketId are mutually exclusive.")
    tid = body.get("tournamentId")
    if tid is None:
        raise EngineError(
            400, "VALIDATION_ERROR", "Invalid request body.", {"tournamentId": ["The mock requires it."]}
        )
    if tid != mx.tid:
        raise EngineError(403, "FORBIDDEN", "Not a member of this tournament.")
    now = mx.clock.now()
    targets = mx.engine.open_orders(profile, now, exchange_id=exchange_id, market_id=market_id)
    paused = mx.faults.take("cancel_all_paused_next")
    stop_at = len(targets) // 2 if paused else len(targets)
    cancelled = 0
    touched: set[str] = set()
    for order in targets[:stop_at]:
        mx.engine.cancel(profile, order.id, now)
        touched.add(order.exchange_id)
        cancelled += 1
    if touched:
        await mx.publish([], touched, [])
    if paused:
        raise EngineError(
            503,
            "SERVICE_UNAVAILABLE",
            "Order cancellation paused because the trading engine is temporarily unavailable.",
            {"cancelled": cancelled, "errors": [], "remaining": len(targets) - stop_at},
        )
    return _json({"cancelled": cancelled, "errors": []})


# ── portfolio ───────────────────────────────────────────────────────────────────────────────


async def get_positions(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    ledger = mx.engine.account(profile).ledger
    rows = []
    totals = {"totalMarketValue": 0.0, "totalCostBasis": 0.0, "totalUnrealizedPnl": 0.0}
    for ex, holding in sorted(ledger.holdings.items()):
        qty = holding.quantity
        if qty == 0:
            continue
        mark = mx.engine.mark(ex)
        if mark is None:
            raise EngineError(409, "CONFLICT", "Tournament holdings exist without valuation prices.")
        m = mx.engine.by_exchange[ex]
        value = holding.market_value_pips(mark) / PIPS_PER_UNIT
        basis = holding.cost_basis_pips / PIPS_PER_UNIT
        shares = abs(qty)
        rows.append(
            {
                "exchangeId": ex,
                "marketId": m.market_id,
                "marketTitle": m.title,
                "option": m.option,
                "settled": False,
                "quantity": qty,
                "avgCost": round(basis / shares, 6),
                "currentPrice": to_price(mark),
                "marketValue": round(value, 4),
                "costBasis": round(basis, 4),
                "unrealizedPnl": round(value - basis, 4),
                "unrealizedPnlPct": round((value - basis) / basis * 100, 4) if basis else 0.0,
                "moneyEarned": holding.realized_pips / PIPS_PER_UNIT,
                "lots": [
                    {
                        "lotId": lot.lot_id,
                        "side": lot.side,
                        "quantity": lot.quantity,
                        "entryPrice": to_price(lot.entry_pips),
                        "openedAt": iso(lot.opened_at),
                    }
                    for lot in holding.lots
                ],
            }
        )
        totals["totalMarketValue"] += value
        totals["totalCostBasis"] += basis
        totals["totalUnrealizedPnl"] += value - basis
    return _json({"positions": rows, "summary": {k: round(v, 4) for k, v in totals.items()}})


async def get_pnl(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    ledger = mx.engine.account(profile).ledger
    marks = dict(mx.engine.state.latest)
    for ex, holding in ledger.holdings.items():
        if holding.quantity and ex not in marks:
            raise EngineError(409, "CONFLICT", "Tournament holdings exist without valuation prices.")
    holdings = ledger.holdings_value_pips(marks) / PIPS_PER_UNIT
    basis = sum(h.cost_basis_pips for h in ledger.holdings.values()) / PIPS_PER_UNIT
    total = ledger.cash + holdings
    return _json(
        {
            "period": request.query_params.get("period", "quarter"),
            "periodStart": None,
            "periodEnd": iso(mx.clock.now()),
            "periodPnl": round(total - 100_000, 4),
            "unrealizedPnl": round(holdings - basis, 4),
            "totalAccountValue": round(total, 4),
            "totalHoldingsValue": round(holdings, 4),
            "totalCostBasis": round(basis, 4),
            "roi": round((total - 100_000) / 100_000 * 100, 4),
            "sharpe": None,
        }
    )


def _fill_json(f: FillRecord) -> dict[str, Any]:
    return {
        "id": f.id,
        "orderId": f.order_id,
        "exchangeId": f.exchange_id,
        "marketId": f.market_id,
        "price": to_price(f.yes_price),
        "quantity": f.quantity,
        "side": f.side,
        "filledAt": iso(f.filled_at),
    }


async def get_fills(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    exchange_id = request.query_params.get("exchangeId")
    market_id = request.query_params.get("marketId")
    fills = [
        f
        for f in reversed(mx.engine.state.fills)
        if f.owner == profile
        and (exchange_id is None or f.exchange_id == exchange_id)
        and (market_id is None or f.market_id == market_id)
    ]
    chunk, pagination = _page(fills, request, default=50, maximum=200)
    pagination.pop("total")
    return _json(
        {
            "data": [_fill_json(f) for f in chunk],
            "pagination": pagination,
            "coverage": {"complete": True, "projectedThroughSequence": len(mx.engine.state.trades)},
        }
    )


def _settlement_json(mx: MockExchange, r: SettlementRecord) -> dict[str, Any]:
    shares = abs(r.quantity)
    return {
        "id": str(r.id),
        "type": "settlement",
        "createdAt": iso(r.settled_at),
        "marketId": r.market_id,
        "marketTitle": mx.engine.markets[r.market_id].title,
        "exchangeId": r.exchange_id,
        "settlementOption": "YES",
        "settlementOutcome": r.outcome,
        "shares": r.quantity,
        "avgEntryPrice": round(r.cost_basis_pips / shares / PIPS_PER_UNIT, 6) if shares else None,
        "costBasis": r.cost_basis_pips / PIPS_PER_UNIT,
        "payout": r.payout_pips / PIPS_PER_UNIT,
        "realizedPnl": (r.payout_pips - r.cost_basis_pips) / PIPS_PER_UNIT,
    }


async def get_settlements(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    rows = [r for r in reversed(mx.engine.state.settlements) if r.owner == profile and r.quantity]
    chunk, pagination = _page(rows, request, default=50, maximum=200)
    pagination.pop("total")
    return _json(
        {
            "data": [_settlement_json(mx, r) for r in chunk],
            "pagination": pagination,
            "coverage": {"complete": True},
        }
    )


async def get_collateral(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    acct = mx.engine.account(profile)
    rows = acct.collateral.views(acct.ledger)
    return _json({"data": [{"profileId": profile, "tournamentId": mx.tid, **row} for row in rows]})


_TX_TYPES = ("trade", "settlement", "deposit", "fee", "market_creation")


def _transactions(mx: MockExchange, profile: str) -> list[dict[str, Any]]:
    """Trades, settlements and ALL collateral events for ``profile``, newest first."""
    eng = mx.engine
    rows: list[tuple[datetime, int, int, dict[str, Any]]] = []
    blank = dict.fromkeys(
        (
            "price",
            "exchangeId",
            "marketId",
            "settlementOption",
            "currentPrice",
            "marketTitle",
            "orderType",
            "contractType",
            "description",
            "amount",
            "transactionType",
            "outstandingAdvanceAfter",
            "componentId",
            "reason",
        )
    )
    for f in eng.state.fills:
        if f.owner != profile:
            continue
        m = eng.by_exchange[f.exchange_id]
        mark = eng.mark(f.exchange_id)
        row = {
            **blank,
            "event_id": f"fill-{f.id}",
            "event_type": "trade",
            "createdAt": iso(f.filled_at),
            "price": to_price(f.yes_price),
            "quantity": f.quantity,
            "exchangeId": f.exchange_id,
            "marketId": m.market_id,
            "marketTitle": m.title,
            "currentPrice": to_price(mark) if mark is not None else None,
            "orderType": "limit",
            "contractType": f.side.upper(),
        }
        rows.append((f.filled_at, 0, f.id, row))
    for r in eng.state.settlements:
        if r.owner != profile or not r.quantity:
            continue
        m = eng.by_exchange[r.exchange_id]
        row = {
            **blank,
            "event_id": f"settlement-{r.id}",
            "event_type": "settlement",
            "createdAt": iso(r.settled_at),
            "quantity": r.quantity,
            "exchangeId": r.exchange_id,
            "marketId": m.market_id,
            "marketTitle": m.title,
            "settlementOption": r.outcome,
            "amount": r.payout_pips / PIPS_PER_UNIT,
        }
        rows.append((r.settled_at, 1, r.id, row))
    for i, e in enumerate(eng.account(profile).collateral.events, start=1):
        m = eng.by_exchange[e.exchange_id]
        # Mock assumption: advances/repayments ride on the trade that caused them, releases on settlement.
        kind = "settlement" if e.type == "ALL_COLLATERAL_RELEASE" else "trade"
        row = {
            **blank,
            "event_id": f"collateral-{i}",
            "event_type": kind,
            "createdAt": iso(e.at),
            "quantity": 0,
            "exchangeId": e.exchange_id,
            "marketId": m.market_id,
            "marketTitle": m.title,
            "amount": e.delta_pips / PIPS_PER_UNIT,
            "transactionType": e.type,
            "outstandingAdvanceAfter": e.outstanding_after_pips / PIPS_PER_UNIT,
            "componentId": e.component_id,
            "reason": e.reason,
        }
        rows.append((e.at, 2, i, row))
    rows.sort(key=lambda r: (r[0], r[1], r[2]), reverse=True)
    return [{**row, "tournamentId": mx.tid} for *_, row in rows]


_COLLATERAL_DETAIL = ("outstandingAdvanceAfter", "componentId", "reason", "tournamentId")


async def _transactions_response(
    mx: MockExchange, request: Request, profile: str, *, collateral_detail: bool = True
) -> Response:
    wanted = request.query_params.get("type")
    rows = _transactions(mx, profile)
    if not collateral_detail:
        rows = [{k: v for k, v in r.items() if k not in _COLLATERAL_DETAIL} for r in rows]
    if wanted:
        kinds = {k.strip() for k in wanted.split(",") if k.strip()}
        if not kinds <= set(_TX_TYPES):
            raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.", {"type": list(_TX_TYPES)})
        rows = [r for r in rows if r["event_type"] in kinds]
    chunk, pagination = _page(rows, request, default=50, maximum=200)
    pagination.pop("total")
    return _json({"data": chunk, "pagination": pagination, "coverage": {"complete": True}})


async def get_transactions(mx: MockExchange, request: Request, profile: str) -> Response:
    """``GET /portfolio/transactions``: the (only) tournament is the key's default."""
    return await _transactions_response(mx, request, profile)


async def get_tournament_transactions(mx: MockExchange, request: Request, profile: str) -> Response:
    """The tournament-scoped read documents no collateral detail fields; only the unscoped one does."""
    mx.require_slug(request)
    return await _transactions_response(mx, request, profile, collateral_detail=False)


async def get_leaderboard(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_slug(request)
    rows = mx.scenario.standings()
    limit = _int_param(request, "limit", 50, 100)
    try:
        offset = int(request.query_params.get("offset") or 0)
    except ValueError as exc:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.", {"offset": ["int"]}) from exc
    my_rank = next((r["rank"] for r in rows if r["profileId"] == profile), None)
    return _json(
        {
            "leaderboard": rows[offset : offset + limit],
            "total": len(rows),
            "period": request.query_params.get("period", "all"),
            "limit": limit,
            "offset": offset,
            "myRank": my_rank,
            "season": None,
            "boundGroups": [],
            "activeGroupId": None,
        }
    )


# ── relationships ───────────────────────────────────────────────────────────────────────────


async def list_relationships(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    type_ = request.query_params.get("type")
    market_id = request.query_params.get("marketId")
    exchange_id = request.query_params.get("exchangeId")
    if market_id and exchange_id:
        raise EngineError(400, "VALIDATION_ERROR", "marketId and exchangeId are mutually exclusive.")
    if market_id:
        m = mx.engine.markets.get(market_id)
        if m is None:
            raise EngineError(404, "NOT_FOUND", "Market not found.")
        exchange_id = m.exchange_id
    rels = [
        r
        for r in mx.scenario.relationships
        if (type_ is None or r.type == type_) and (exchange_id is None or exchange_id in r.exchange_ids)
    ]
    chunk, pagination = _page(rels, request, default=100, maximum=200)
    pagination.pop("total")
    return _json({"data": [mx.scenario.relationship_json(r) for r in chunk], "pagination": pagination})


async def get_graph(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    market_id = request.query_params.get("marketId")
    m = mx.engine.markets.get(market_id or "")
    if m is None:
        raise EngineError(404, "NOT_FOUND", "Market not found.")
    touching = [r for r in mx.scenario.relationships if m.exchange_id in r.exchange_ids]
    rels = [mx.scenario.relationship_json(r) for r in touching]
    nodes = {n["exchangeId"]: n for r in rels for n in r["nodes"]}
    return _json({"relationships": rels, "nodes": list(nodes.values())})


async def get_constraints(mx: MockExchange, request: Request, profile: str) -> Response:
    mx.require_tournament(request)
    violations_only = request.query_params.get("violationsOnly", "false") == "true"
    try:
        min_violation = float(request.query_params.get("minViolation") or 0.01)
    except ValueError as exc:
        raise EngineError(400, "VALIDATION_ERROR", "Invalid query parameters.") from exc
    rel_id = request.query_params.get("relationshipId")
    market_id = request.query_params.get("marketId")
    exchange = mx.engine.markets[market_id].exchange_id if market_id in mx.engine.markets else None
    if market_id and exchange is None:
        raise EngineError(404, "NOT_FOUND", "Market not found.")
    rows = []
    for rel in mx.scenario.relationships:
        if rel_id and rel.id != rel_id:
            continue
        if exchange and exchange not in rel.exchange_ids:
            continue
        ev = mx.scenario.evaluate(rel, min_violation)
        if violations_only and ev["evaluationStatus"] != "violated":
            continue
        rows.append(ev)
    return _json(
        {
            "data": rows,
            "violationsCount": sum(1 for r in rows if r["evaluationStatus"] == "violated"),
            "computedAt": iso(mx.clock.now()),
        }
    )


async def post_realtime_token(mx: MockExchange, request: Request, profile: str) -> Response:
    token = f"mock-rt-{secrets.token_hex(12)}"
    expires = mx.clock.now() + REALTIME_TTL
    mx.tokens[token] = TokenInfo(profile, expires)
    return _json(
        {
            "token": token,
            "expiresAt": iso(expires),
            "supabaseUrl": mx.config.public_origin,
            "anonKey": mx.hub.anon_key,
            "channels": {"user": f"user:{profile}"},
        }
    )


# ── admin (/_mock) ──────────────────────────────────────────────────────────────────────────


def admin(fn: Callable[[MockExchange, Any], Awaitable[Any]]) -> Callable[[Request], Awaitable[Response]]:
    async def endpoint(request: Request) -> Response:
        mx: MockExchange = request.app.state.mx
        try:
            body = await _body(request) if request.method == "POST" else dict(request.query_params)
            return _json(await fn(mx, body))
        except (EngineError, KeyError, ValueError, TypeError) as err:
            message = err.message if isinstance(err, EngineError) else f"{type(err).__name__}: {err}"
            return _json({"error": {"code": "MOCK_ADMIN_ERROR", "message": message}}, 400)

    return endpoint


async def admin_state(mx: MockExchange, _: Any) -> Any:
    now = mx.clock.now()
    eng = mx.engine
    return {
        "seed": mx.config.seed,
        "now": iso(now),
        "tournamentId": mx.tid,
        "slug": mx.config.tournament_slug,
        "fair": {ex: to_price(p) for ex, p in sorted(mx.scenario.fair.items())},
        "markets": [
            {
                "marketId": m.market_id,
                "exchangeId": m.exchange_id,
                "title": m.title,
                "details": m.contract_details,
            }
            for m in eng.markets.values()
        ],
        "relationships": [
            {"id": r.id, "type": r.type, "exchangeIds": r.exchange_ids, "isExhaustive": r.is_exhaustive}
            for r in mx.scenario.relationships
        ],
        "accounts": {
            a.username: {
                "profileId": a.profile_id,
                "cash": a.ledger.cash,
                "positions": {ex: h.quantity for ex, h in a.ledger.holdings.items() if h.quantity},
                "openOrders": len(eng.open_orders(a.profile_id, now)),
                "collateral": a.collateral.views(a.ledger),
            }
            for a in eng.state.accounts.values()
        },
        "collateral": eng.collateral_enabled,
        "faults": mx.faults.as_dict(),
        "realtime": {"connections": mx.hub.connections, "joins": mx.hub.joins},
    }


async def admin_reset(mx: MockExchange, body: Any) -> Any:
    mx.reset(int(body.get("seed", mx.config.seed)))
    return {"ok": True, "seed": mx.config.seed}


async def admin_faults(mx: MockExchange, body: Any) -> Any:
    if body:
        mx.faults.update(body)
    return mx.faults.as_dict()


async def admin_clock(mx: MockExchange, body: Any) -> Any:
    if "set" in body:
        mx.clock.set(parse_iso(str(body["set"])))
    if "advanceSeconds" in body:
        mx.clock.advance(float(body["advanceSeconds"]))
    return {"now": iso(mx.clock.now())}


async def admin_fair(mx: MockExchange, body: Any) -> Any:
    ex = str(body["exchangeId"])
    if ex not in mx.engine.by_exchange:
        raise KeyError(ex)
    mx.scenario.move_fair(ex, to_pips(float(body["fair"])), mx.clock.now())
    await mx.publish([], {ex}, [])
    return {"exchangeId": ex, "fair": to_price(mx.scenario.fair[ex])}


async def admin_sponsor(mx: MockExchange, body: Any) -> Any:
    hw = body.get("halfWidth")
    size = body.get("size")
    mx.scenario.set_sponsor(to_pips(float(hw)) if hw is not None else None, size, mx.clock.now())
    await mx.publish([], set(mx.engine.by_exchange), [])
    cfg = mx.scenario.config
    return {"halfWidthPips": cfg.sponsor_half_width_pips, "size": cfg.sponsor_size}


async def admin_stale(mx: MockExchange, body: Any) -> Any:
    ex = str(body["exchangeId"])
    side = str(body.get("side", "bid"))
    if side not in ("bid", "ask"):
        raise ValueError("side must be bid or ask")
    through = to_pips(float(body.get("through", 0.05)))
    order_id = mx.scenario.inject_stale(ex, side, through, int(body.get("quantity", 50)), mx.clock.now())
    await mx.publish([], {ex}, [])
    return {"orderId": order_id}


async def admin_violation(mx: MockExchange, body: Any) -> Any:
    amount = to_pips(float(body.get("amount", 0.06)))
    rel = mx.scenario.create_violation(body.get("relationshipId"), amount, mx.clock.now())
    await mx.publish([], set(rel.exchange_ids), [])
    await mx.broadcast_violations(set(rel.exchange_ids))
    return {"relationshipId": rel.id, "evaluation": mx.scenario.evaluate(rel)}


async def admin_settle(mx: MockExchange, body: Any) -> Any:
    ex = str(body["exchangeId"])
    outcome = str(body.get("outcome", "")).upper()
    records = mx.engine.settle(ex, outcome, mx.clock.now())
    await mx.publish([], {ex}, [])
    return {
        "exchangeId": ex,
        "outcome": outcome,
        "settled": [
            {
                "profileId": r.owner,
                "quantity": r.quantity,
                "payout": r.payout_pips / PIPS_PER_UNIT,
                "released": r.released_pips / PIPS_PER_UNIT,
            }
            for r in records
        ],
    }


async def admin_rt_disconnect(mx: MockExchange, _: Any) -> Any:
    return {"closed": await mx.hub.disconnect_all()}


async def admin_rt_skip(mx: MockExchange, body: Any) -> Any:
    channel = str(body.get("channel") or f"tournament:{mx.tid}")
    mx.hub.skip_revision(channel, int(body.get("count", 1)))
    return {"channel": channel}


async def admin_rt_broadcast(mx: MockExchange, body: Any) -> Any:
    sent = await mx.hub.broadcast(str(body["channel"]), str(body["event"]), dict(body.get("payload") or {}))
    return {"sent": sent}


async def realtime_ws(ws: WebSocket) -> None:
    await ws.app.state.mx.hub.serve(ws)


def create_app(config: MockConfig | None = None) -> Starlette:
    mx = MockExchange(config or MockConfig())
    routes = [
        Route(f"{API}/account", api(get_account)),
        Route(f"{API}/tournaments", api(list_tournaments)),
        Route(f"{API}/tournaments/{{slug}}", api(get_tournament)),
        Route(f"{API}/tournaments/{{slug}}/markets", api(list_markets)),
        Route(f"{API}/tournaments/{{slug}}/markets/{{id}}", api(get_market)),
        Route(f"{API}/tournaments/{{slug}}/portfolio/positions", api(get_positions)),
        Route(f"{API}/tournaments/{{slug}}/portfolio/pnl", api(get_pnl)),
        Route(f"{API}/tournaments/{{slug}}/portfolio/fills", api(get_fills)),
        Route(f"{API}/tournaments/{{slug}}/portfolio/settlements", api(get_settlements)),
        Route(f"{API}/tournaments/{{slug}}/leaderboard", api(get_leaderboard)),
        Route(f"{API}/markets/{{id}}/nodes", api(get_nodes)),
        Route(f"{API}/exchanges/prices", api(get_prices)),
        Route(f"{API}/exchanges/{{id}}/price", api(get_price)),
        Route(f"{API}/exchanges/{{id}}/orderbook", api(get_orderbook)),
        Route(f"{API}/exchanges/{{id}}/trades", api(get_trades)),
        Route(f"{API}/exchanges/{{id}}/price-history", api(get_price_history)),
        Route(f"{API}/orders", api(post_order), methods=["POST"]),
        Route(f"{API}/orders", api(list_orders), methods=["GET"]),
        Route(f"{API}/orders/batch", api(post_batch), methods=["POST"]),
        Route(f"{API}/orders/multi-leg", api(post_multi_leg), methods=["POST"]),
        Route(f"{API}/orders/cancel-all", api(cancel_all), methods=["POST"]),
        Route(f"{API}/orders/{{id:int}}", api(get_order), methods=["GET"]),
        Route(f"{API}/orders/{{id:int}}", api(delete_order), methods=["DELETE"]),
        Route(f"{API}/orders/{{id:int}}/fills", api(get_order_fills), methods=["GET"]),
        Route(f"{API}/portfolio/collateral", api(get_collateral)),
        Route(f"{API}/portfolio/transactions", api(get_transactions)),
        Route(f"{API}/tournaments/{{slug}}/portfolio/transactions", api(get_tournament_transactions)),
        Route(f"{API}/relationships", api(list_relationships)),
        Route(f"{API}/relationships/graph", api(get_graph)),
        Route(f"{API}/relationships/constraints", api(get_constraints)),
        Route(f"{API}/realtime/token", api(post_realtime_token), methods=["POST"]),
        WebSocketRoute("/realtime/v1/websocket", realtime_ws),
        Route("/_mock/state", admin(admin_state), methods=["GET"]),
        Route("/_mock/reset", admin(admin_reset), methods=["POST"]),
        Route("/_mock/faults", admin(admin_faults), methods=["GET", "POST"]),
        Route("/_mock/clock", admin(admin_clock), methods=["POST"]),
        Route("/_mock/fair", admin(admin_fair), methods=["POST"]),
        Route("/_mock/sponsor", admin(admin_sponsor), methods=["POST"]),
        Route("/_mock/stale-order", admin(admin_stale), methods=["POST"]),
        Route("/_mock/violation", admin(admin_violation), methods=["POST"]),
        Route("/_mock/settle", admin(admin_settle), methods=["POST"]),
        Route("/_mock/realtime/disconnect", admin(admin_rt_disconnect), methods=["POST"]),
        Route("/_mock/realtime/skip-revision", admin(admin_rt_skip), methods=["POST"]),
        Route("/_mock/realtime/broadcast", admin(admin_rt_broadcast), methods=["POST"]),
    ]
    app = Starlette(routes=routes)
    app.state.mx = mx
    return app


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="susq-mock", description="Faithful mock of The Super Market API.")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument(
        "--api-key",
        action="append",
        default=[],
        metavar="KEY[=USERNAME]",
        help="accepted API key (repeatable); default mock-key-alpha=trader-alpha",
    )
    parser.add_argument("--rate-limit-rps", type=float, default=None)
    parser.add_argument("--retry-after-s", type=int, default=1)
    parser.add_argument("--random-503", type=float, default=0.0, help="probability of a random 503")
    parser.add_argument("--latency-ms", type=int, default=0)
    parser.add_argument("--sponsor-half-width", type=float, default=0.01)
    parser.add_argument("--sponsor-size", type=int, default=500)
    parser.add_argument("--players", type=int, default=40, help="synthetic leaderboard entrants")
    parser.add_argument(
        "--no-collateral",
        action="store_true",
        help="disable ALL collateral modeling (plain cash accounting, `all: null`)",
    )
    return parser


def config_from_args(argv: list[str] | None = None) -> MockConfig:
    """Parse ``susq-mock`` command-line flags into a :class:`MockConfig`."""
    args = _parser().parse_args(argv)

    keys: dict[str, str] = {}
    for item in args.api_key or ["mock-key-alpha=trader-alpha"]:
        key, _, name = item.partition("=")
        keys[key] = name or f"trader-{len(keys)}"
    return MockConfig(
        seed=args.seed,
        api_keys=keys,
        sponsor_half_width_pips=to_pips(args.sponsor_half_width),
        sponsor_size=args.sponsor_size,
        synthetic_players=args.players,
        collateral=not args.no_collateral,
        faults=Faults(
            rate_limit_rps=args.rate_limit_rps,
            retry_after_s=args.retry_after_s,
            random_503_prob=args.random_503,
            latency_ms=args.latency_ms,
        ),
        public_origin=f"http://{args.host}:{args.port}",
    )


def main(argv: list[str] | None = None) -> None:
    """``susq-mock``: serve the mock exchange."""
    import uvicorn

    args = _parser().parse_args(argv)
    uvicorn.run(create_app(config_from_args(argv)), host=args.host, port=args.port, log_level="info")
