"""The gateway: the ONLY path from the bridge to the platform for writes (and the metered path for reads).

Every write runs the same gauntlet, in this order:

1. **kill** - ``ops/KILL`` present -> cancel-all (once), freeze, refuse;
2. **lease** - only the holder of ``ops/state/trader.lease`` may place or cancel;
3. **breakers** - a tripped risk breaker refuses new orders (cancels still flow);
4. **mode** - paper routes to the :class:`~susq.platform.paper.PaperBroker`, live to the client;
5. **time gate** - no new orders at/after ``calendar.last_order_utc``; every order carries
   ``expirationDate = min(now + ttl, last_order_utc)``;
6. **risk** pre-check (may shrink the order);
7. **write-ahead intent** + journal event, *then* the request;
8. **retry policy** - only documented codes (429, 409 REQUEST_IN_FLIGHT >= 90 s, 502, 503, 5xx and
   network errors), always with the same idempotency key, exponential backoff with jitter,
   never past the order's own expiry;
9. post-process: intent state, OMS tracking, journal, and the ``all`` (ALL collateral) economics of
   every response, recorded whatever ``[risk] use_collateral_buying_power`` says.
"""

from __future__ import annotations

import asyncio
import random
import time
from collections import deque
from collections.abc import Awaitable, Callable, Sequence
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING, Any, Literal

from susq.contracts import ops_pb2
from susq.platform.client import SuperMarketClient, TradingApi
from susq.platform.errors import REQUEST_IN_FLIGHT_WAIT_S, ApiError, NetworkError, PlatformError, RetryClass
from susq.platform.models import (
    CancelAllResult,
    Order,
    OrderBook,
    OrderRequest,
    PlaceOrderResult,
    Relationship,
)
from susq.platform.paper import PaperBroker
from susq.platform.prices import to_pips

from susq_bridge.collateral import CollateralTracker, request_legs
from susq_bridge.config import BridgeConfig
from susq_bridge.intents import IntentStore
from susq_bridge.journal import Journal
from susq_bridge.killswitch import KillSwitch
from susq_bridge.lease import Lease
from susq_bridge.limiter import AdaptiveLimiter
from susq_bridge.risk import RiskEngine

if TYPE_CHECKING:
    from susq_bridge.oms import Oms

__all__ = ["BundleOutcome", "Gateway", "GatewayBooks", "OrderOutcome", "OrderSpec", "Portfolio"]

Status = Literal["placed", "risk_rejected", "platform_rejected", "blocked", "unknown", "duplicate"]
MIN_EXPIRY_S = 2.0


@dataclass(frozen=True, slots=True)
class OrderSpec:
    """A strategy's order, before the gateway stamps expiry, tournament and idempotency."""

    exchange_id: str
    side: Literal["yes", "no"]
    action: Literal["buy", "sell"]
    quantity: int
    price_pips: int
    ttl_s: float | None = None
    fair_pips: int | None = None  # enables the glitch cap
    input_ts: float | None = None  # unix time of the newest input the decision used
    max_notional: float | None = None  # strategy-specific per-order cap (SUSQies)


@dataclass
class OrderOutcome:
    intent_id: str
    status: Status
    request: OrderRequest | None = None
    result: PlaceOrderResult | None = None
    reason: str | None = None
    idempotency_key: str = ""
    attempts: int = 0

    @property
    def placed(self) -> bool:
        return self.status == "placed"


@dataclass
class BundleOutcome:
    intent_id: str
    status: Status
    requests: list[OrderRequest] = field(default_factory=list)
    results: list[PlaceOrderResult] = field(default_factory=list)
    reason: str | None = None
    idempotency_key: str = ""
    attempts: int = 0

    @property
    def placed(self) -> bool:
        return self.status == "placed"


@dataclass(frozen=True, slots=True)
class Portfolio:
    open_orders: list[Order]
    positions: dict[str, int]
    marks: dict[str, int]
    equity: float
    cash: float | None = None


class GatewayBooks:
    """Book source for the paper broker that reads through the gateway (limiter + journal)."""

    def __init__(self, gateway: Gateway) -> None:
        self._gateway = gateway

    async def get_orderbook(self, exchange_id: str, tournament_id: str, *, depth: int = 20) -> OrderBook:
        client = self._gateway.client
        return await self._gateway.read(
            "orderbook", lambda: client.get_orderbook(exchange_id, tournament_id, depth=depth)
        )


class Gateway:
    def __init__(
        self,
        config: BridgeConfig,
        *,
        client: SuperMarketClient,
        mode: Literal["paper", "live"],
        tournament_id: str,
        journal: Journal,
        intents: IntentStore,
        lease: Lease,
        kill: KillSwitch,
        risk: RiskEngine,
        limiter: AdaptiveLimiter,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
        sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
        rng: random.Random | None = None,
        max_write_attempts: int = 6,
        paper_cash: float = 100_000.0,
    ) -> None:
        self.config = config
        self.client = client
        self.mode = mode
        self.tournament_id = tournament_id
        self.journal = journal
        self.intents = intents
        self.lease = lease
        self.kill = kill
        self.risk = risk
        self.limiter = limiter
        self.clock = clock
        self._sleep = sleep
        self._rng = rng or random.Random()  # noqa: S311 - jitter, not crypto
        self.max_write_attempts = max_write_attempts
        self.paper: PaperBroker | None = None
        if mode == "paper":
            self.paper = PaperBroker(GatewayBooks(self), tournament_id, starting_cash=paper_cash, clock=clock)
        self.broker: TradingApi = self.paper if self.paper is not None else client
        self.oms: Oms | None = None
        self.kill_handled = False
        #: Fills from our own placements (for the status snapshot) and human steps we can't do.
        self.recent_trades: deque[dict[str, Any]] = deque(maxlen=100)
        self.action_needed: list[str] = []
        self._background: set[asyncio.Task[Any]] = set()
        risk.on_trip = self._on_trip
        #: ALL collateral: real economics from every response + the model behind the opt-in sizing.
        self.collateral = CollateralTracker()
        risk.cost_model = lambda reqs: self.collateral.effective_cost_pips(risk.positions, request_legs(reqs))

    def attach(self, oms: Oms) -> None:
        self.oms = oms

    def set_relationships(self, relationships: Sequence[Relationship]) -> None:
        """Feed ``GET /relationships`` to the collateral model (and to the paper broker's copy)."""
        relations = self.collateral.set_relationships(relationships)
        if self.paper is not None:
            self.paper.set_relationships(relations)

    # ── gates ───────────────────────────────────────────────────────────────────────────────

    async def gate(self) -> str | None:
        """Why a new order may not be sent right now (None = go)."""
        if self.kill.killed():
            await self.enforce_kill()
            return f"killed: {self.kill.reason()}"
        if not self.lease.held():
            return "no_lease"
        if self.risk.frozen:
            return f"frozen: {self.risk.frozen_reason}"
        return None

    def expiration_for(self, now: datetime, ttl_s: float | None) -> datetime | None:
        """``min(now + ttl, last_order_utc)`` at whole seconds, or None when the time gate is shut."""
        last = self.config.calendar.last_order_utc
        if now >= last:
            return None
        ttl = self.config.oms.default_ttl_s if ttl_s is None else ttl_s
        expiry = min(now + timedelta(seconds=ttl), last).replace(microsecond=0)
        if (expiry - now).total_seconds() < MIN_EXPIRY_S:
            return None
        return expiry

    async def enforce_kill(self) -> None:
        """Kill beats everything: freeze, then cancel-all once (scoped to the tournament)."""
        if self.kill_handled:
            return
        self.kill_handled = True
        reason = self.kill.reason() or "killed"
        self.journal.event("kill", {"reason": reason})
        self.risk.trip(f"kill: {reason}")
        await self.cancel_all(f"kill: {reason}")

    def _on_trip(self, reason: str) -> None:
        self.journal.event("breaker", {"reason": reason})
        if reason.startswith("kill:"):
            return  # enforce_kill cancels itself
        task = asyncio.get_running_loop().create_task(self.cancel_all(f"breaker: {reason}"))
        self._background.add(task)
        task.add_done_callback(self._background.discard)

    # ── reads ───────────────────────────────────────────────────────────────────────────────

    async def read[T](
        self, label: str, fn: Callable[[], Awaitable[T]], *, attempts: int = 4, priority: bool = False
    ) -> T:
        """A metered, retried read. Terminal errors raise immediately.

        ``priority`` reads sit on the critical path to an order (the racer's books) and may draw from
        the reserved order budget, so background reads (recorder, census) can't delay them.
        """
        backoff = 0.25
        for attempt in range(1, attempts + 1):
            await self.limiter.acquire("write" if priority else "read")
            try:
                value = await fn()
            except ApiError as err:
                if err.retry_class is RetryClass.TERMINAL or attempt == attempts:
                    raise
                wait = self._wait_for(err, backoff)
            except NetworkError:
                if attempt == attempts:
                    raise
                wait = backoff * self._rng.uniform(0.5, 1.5)
            else:
                self.limiter.on_success()
                return value
            await self._sleep(wait)
            backoff = min(backoff * 2, 8.0)
        raise AssertionError("unreachable")  # pragma: no cover

    def _wait_for(self, err: ApiError, backoff: float) -> float:
        if err.code == "RATE_LIMITED" or err.status == 429:
            self.limiter.on_rate_limited(err.retry_after_s)
            return max(err.retry_after_s or 1.0, backoff)
        if err.code == "REQUEST_IN_FLIGHT":
            return max(REQUEST_IN_FLIGHT_WAIT_S, err.retry_after_s or 0.0)
        return backoff * self._rng.uniform(0.5, 1.5)

    # ── write core ──────────────────────────────────────────────────────────────────────────

    async def _send[T](
        self,
        label: str,
        key: str,
        intent_ids: Sequence[str],
        attempt: Callable[[], Awaitable[T]],
        deadline: datetime,
    ) -> tuple[T | None, Status, str | None, int]:
        """Send with the documented retry policy. Same idempotency key on every attempt."""
        backoff = 0.25
        attempts = 0
        last_code: str | None = None
        while True:
            if self.kill.killed():
                await self.enforce_kill()
                return None, ("unknown" if attempts else "blocked"), "killed"
            attempts += 1
            for iid in intent_ids:
                self.intents.update(iid, ops_pb2.INTENT_STATE_SENT, bump_attempts=True)
            if self.mode == "live":
                await self.limiter.acquire("write")
            try:
                value = await attempt()
            except ApiError as err:
                last_code = err.code
                self.journal.event(
                    "write_error",
                    {"label": label, "status": err.status, "code": err.code, "attempt": attempts},
                    idempotency_key=key,
                )
                if err.retry_class is RetryClass.TERMINAL:
                    if err.code == "TERMS_NOT_ACKNOWLEDGED":
                        note = "TERMS_NOT_ACKNOWLEDGED: accept the updated Terms on the website"
                        if note not in self.action_needed:
                            self.action_needed.append(note)
                    for iid in intent_ids:
                        self.intents.update(iid, ops_pb2.INTENT_STATE_REJECTED, error_code=err.code)
                    self.risk.record_reject()
                    return None, "platform_rejected", err.code, attempts
                for iid in intent_ids:
                    self.intents.update(iid, ops_pb2.INTENT_STATE_UNKNOWN, error_code=err.code)
                wait = self._wait_for(err, backoff)
            except NetworkError as err:
                last_code = "NETWORK"
                self.journal.event("write_error", {"label": label, "network": str(err)}, idempotency_key=key)
                for iid in intent_ids:
                    self.intents.update(iid, ops_pb2.INTENT_STATE_UNKNOWN, error_code="NETWORK")
                wait = backoff * self._rng.uniform(0.5, 1.5)
            else:
                if self.mode == "live":
                    self.limiter.on_success()
                return value, "placed", None, attempts
            if attempts >= self.max_write_attempts or self.clock() + timedelta(seconds=wait) >= deadline:
                return None, "unknown", last_code, attempts
            await self._sleep(wait)
            backoff = min(backoff * 2, 8.0)

    def _request(self, spec: OrderSpec, expiration: datetime, quantity: int | None = None) -> OrderRequest:
        return OrderRequest(
            exchange_id=spec.exchange_id,
            side=spec.side,
            action=spec.action,
            quantity=spec.quantity if quantity is None else quantity,
            price_pips=spec.price_pips,
            expiration=expiration,
            tournament_id=self.tournament_id,
        )

    # ── single orders ───────────────────────────────────────────────────────────────────────

    async def place(self, intent_id: str, strategy: str, spec: OrderSpec) -> OrderOutcome:
        existing = self.intents.get(intent_id)
        if existing is not None:
            return OrderOutcome(intent_id, "duplicate", reason="intent already exists")
        if (why := await self.gate()) is not None:
            return OrderOutcome(intent_id, "blocked", reason=why)
        now = self.clock()
        expiration = self.expiration_for(now, spec.ttl_s)
        if expiration is None:
            return OrderOutcome(intent_id, "blocked", reason="time_gate")
        request = self._request(spec, expiration)
        assert self.oms is not None
        decision = self.risk.check(
            request, self.oms.open_orders(), fair_pips=spec.fair_pips, max_notional=spec.max_notional
        )
        if not decision.accepted:
            self.journal.event(
                "risk_reject", {"intent": intent_id, "reason": decision.reason, **decision.detail}
            )
            return OrderOutcome(intent_id, "risk_rejected", request, reason=decision.reason)
        if decision.quantity != request.quantity:
            request = replace(request, quantity=decision.quantity)
        self.risk.commit(request, decision)
        intent = self.intents.create(intent_id, strategy, request)
        key = intent.idempotency_key
        self.journal.event(
            "order_intent",
            {
                "intent": intent_id,
                "strategy": strategy,
                "payload": request.canonical_payload(),
                "mode": self.mode,
            },
            idempotency_key=key,
        )
        result, status, reason, attempts = await self._send(
            "place_order",
            key,
            [intent_id],
            lambda: self.broker.place_order(request, key),
            request.expiration - timedelta(seconds=1),
        )
        if result is not None:
            self._acked(intent_id, strategy, request, result, spec.input_ts)
        return OrderOutcome(intent_id, status, request, result, reason, key, attempts)

    def _acked(
        self,
        intent_id: str,
        strategy: str,
        request: OrderRequest,
        result: PlaceOrderResult,
        input_ts: float | None,
    ) -> None:
        state = ops_pb2.INTENT_STATE_ACKED if result.open else ops_pb2.INTENT_STATE_DONE
        self.intents.update(intent_id, state, order_id=result.order_id or 0)
        if (econ := self.collateral.observe(intent_id, request, result)) is not None:
            self.journal.event("all_economics", {**econ.journal(), "strategy": strategy, "mode": self.mode})
        if self.oms is not None:
            self.oms.on_placed(intent_id, strategy, request, result, input_ts)
        if result.quantity_traded:
            self.recent_trades.append(
                {
                    "ts_ns": time.time_ns(),
                    "strategy": strategy,
                    "exchange_id": request.exchange_id,
                    "side": result.side or request.side,
                    "action": result.action or request.action,
                    "quantity": round(result.quantity_traded),
                    "price": result.fill_price or 0.0,
                }
            )
        self.journal.event(
            "order_ack",
            {
                "intent": intent_id,
                "orderId": result.order_id,
                "open": result.open,
                "traded": result.quantity_traded,
                "canonical": [result.action, result.side, result.price, result.quantity],
                "terminalReasonCode": result.terminal_reason_code,
            },
        )

    # ── bundles (atomic multi-leg) ──────────────────────────────────────────────────────────

    async def place_bundle(
        self, intent_id: str, strategy: str, specs: Sequence[OrderSpec], *, ttl_s: float | None = None
    ) -> BundleOutcome:
        """All-or-nothing legs of equal quantity via ``/orders/multi-leg`` (never relationshipConstraint)."""
        if self.intents.get(f"{intent_id}#0") is not None:
            return BundleOutcome(intent_id, "duplicate", reason="intent already exists")
        if (why := await self.gate()) is not None:
            return BundleOutcome(intent_id, "blocked", reason=why)
        now = self.clock()
        expiration = self.expiration_for(now, ttl_s)
        if expiration is None:
            return BundleOutcome(intent_id, "blocked", reason="time_gate")
        assert self.oms is not None
        quantity = min(s.quantity for s in specs)
        requests: list[OrderRequest] = []
        for _ in range(2):  # a shrink on one leg shrinks all legs; the second pass must accept as-is
            requests, reasons = [self._request(s, expiration, quantity) for s in specs], []
            pending: list[Any] = list(self.oms.open_orders())
            allowed = quantity
            for i, (spec, req) in enumerate(zip(specs, requests, strict=True)):
                decision = self.risk.check(
                    req,
                    pending,
                    fair_pips=spec.fair_pips,
                    max_notional=spec.max_notional,
                    prior_legs=requests[:i],
                )
                if not decision.accepted:
                    reasons.append(decision.reason)
                    allowed = 0
                    break
                allowed = min(allowed, decision.quantity)
                pending.append(_PendingView(req))
            if allowed < 1:
                self.journal.event("risk_reject", {"intent": intent_id, "reason": reasons})
                return BundleOutcome(intent_id, "risk_rejected", requests, reason=str(reasons or "cap"))
            if allowed == quantity:
                break
            quantity = allowed
        else:
            return BundleOutcome(intent_id, "risk_rejected", requests, reason="unstable sizing")
        key = self.intents.bundle_key(intent_id, requests)
        leg_ids = [f"{intent_id}#{i}" for i in range(len(requests))]
        for leg_id, req in zip(leg_ids, requests, strict=True):
            self.intents.create(leg_id, strategy, req, idempotency_key=key)
        self.journal.event(
            "bundle_intent",
            {"intent": intent_id, "strategy": strategy, "legs": [r.canonical_payload() for r in requests]},
            idempotency_key=key,
        )
        result, status, reason, attempts = await self._send(
            "place_multi_leg",
            key,
            leg_ids,
            lambda: self.broker.place_multi_leg(requests, key),
            expiration - timedelta(seconds=1),
        )
        results: list[PlaceOrderResult] = []
        if result is not None:
            results = [item.data for item in sorted(result.results, key=lambda it: it.index)]
            input_ts = max((s.input_ts for s in specs if s.input_ts is not None), default=None)
            for leg_id, req, res in zip(leg_ids, requests, results, strict=True):
                self._acked(leg_id, strategy, req, res, input_ts)
        return BundleOutcome(intent_id, status, requests, results, reason, key, attempts)

    # ── restart recovery ────────────────────────────────────────────────────────────────────

    async def resume_intents(self, *, max_age_s: float = 30.0) -> list[BundleOutcome]:
        """Retry fresh unresolved intents with their original key; close out stale ones.

        A crash can leave intents PENDING (never sent), SENT, or UNKNOWN (maybe placed). Re-sending
        the identical payload with the same key is always safe: the platform replays a stored
        response instead of placing twice. Stale decisions are *not* re-sent; the reconcile loop
        picks up whatever they did through open orders and positions.
        """
        outcomes: list[BundleOutcome] = []
        groups: dict[str, list[ops_pb2.OrderIntent]] = {}
        for intent in self.intents.unresolved():
            groups.setdefault(intent.idempotency_key, []).append(intent)
        now = self.clock()
        for key, group in groups.items():
            group.sort(key=lambda i: i.intent_id)
            requests = [IntentStore.to_request(i) for i in group]
            ids = [i.intent_id for i in group]
            age = (now.timestamp() * 1e9 - min(i.created_at_ns for i in group)) / 1e9
            earliest = min(r.expiration for r in requests)
            fresh = age <= max_age_s and earliest - now > timedelta(seconds=MIN_EXPIRY_S)
            if not fresh or (why := await self.gate()) is not None:
                reason = "stale_on_restart" if not fresh else f"blocked_on_restart: {why}"
                for iid in ids:
                    self.intents.update(iid, ops_pb2.INTENT_STATE_DONE, error_code=reason)
                self.journal.event("intent_closed", {"intents": ids, "reason": reason}, idempotency_key=key)
                outcomes.append(
                    BundleOutcome(ids[0], "blocked", requests, reason=reason, idempotency_key=key)
                )
                continue
            strategy = group[0].strategy
            if len(requests) == 1:
                req = requests[0]
                single, status, reason, attempts = await self._send(
                    "resume_order",
                    key,
                    ids,
                    lambda req=req, key=key: self.broker.place_order(req, key),
                    req.expiration,
                )
                results = [single] if single is not None else []
            else:
                multi, status, reason, attempts = await self._send(
                    "resume_multi_leg",
                    key,
                    ids,
                    lambda reqs=requests, key=key: self.broker.place_multi_leg(reqs, key),
                    earliest,
                )
                results = [it.data for it in sorted(multi.results, key=lambda it: it.index)] if multi else []
            for iid, req, res in zip(ids, requests, results, strict=False):
                self._acked(iid, strategy, req, res, None)
            outcomes.append(BundleOutcome(ids[0], status, requests, results, reason, key, attempts))
        return outcomes

    # ── cancels ─────────────────────────────────────────────────────────────────────────────

    async def cancel(self, order_id: int, *, attempts: int = 4) -> bool:
        """Cancel one order. True once it is certainly not resting (cancelled, filled or unknown id)."""
        if not self.lease.held():
            return False
        backoff = 0.25
        for attempt in range(1, attempts + 1):
            if self.mode == "live":
                await self.limiter.acquire("write")
            try:
                await self.broker.cancel_order(order_id)
            except ApiError as err:
                if err.status in (404, 409):  # gone or already closed: the goal is met
                    break
                if err.retry_class is RetryClass.TERMINAL or attempt == attempts:
                    self.journal.event("cancel_failed", {"orderId": order_id, "code": err.code})
                    return False
                await self._sleep(self._wait_for(err, backoff))
            except NetworkError:
                if attempt == attempts:
                    return False
                await self._sleep(backoff)
            else:
                break
            backoff = min(backoff * 2, 8.0)
        if self.oms is not None:
            self.oms.on_cancelled(order_id)
        self.journal.event("cancelled", {"orderId": order_id})
        return True

    async def cancel_all(self, reason: str, *, exchange_id: str | None = None, attempts: int = 5) -> bool:
        """Tournament-scoped cancel-all, confirmed against ``GET /orders?status=open``."""
        if not self.lease.held():
            self.journal.event("cancel_all_skipped", {"reason": reason, "why": "no_lease"})
            return False
        self.journal.event("cancel_all", {"reason": reason, "exchangeId": exchange_id})
        backoff = 0.25
        result: CancelAllResult | None = None
        for attempt in range(1, attempts + 1):
            try:
                result = await self.broker.cancel_all(self.tournament_id, exchange_id=exchange_id)
                break
            except PlatformError as err:
                if isinstance(err, ApiError) and err.retry_class is RetryClass.TERMINAL:
                    break
                if attempt == attempts:
                    break
                await self._sleep(backoff * self._rng.uniform(0.5, 1.5))
                backoff = min(backoff * 2, 8.0)
        # 207/422/503/unknown: confirm what is still resting and cancel it one by one.
        remaining = await self.read(
            "open_orders", lambda: self.broker.list_open_orders(self.tournament_id, exchange_id=exchange_id)
        )
        for order in remaining:
            await self.cancel(order.id)
        leftover = await self.read(
            "open_orders", lambda: self.broker.list_open_orders(self.tournament_id, exchange_id=exchange_id)
        )
        if self.oms is not None:
            self.oms.forget_all(exchange_id)
        ok = not leftover
        self.journal.event(
            "cancel_all_done",
            {"reason": reason, "cancelled": result.cancelled if result else None, "leftover": len(leftover)},
        )
        return ok

    # ── portfolio ───────────────────────────────────────────────────────────────────────────

    async def portfolio(self, marks: dict[str, int]) -> Portfolio:
        """Authoritative open orders, positions and equity (paper state in paper mode)."""
        slug = self.config.platform.tournament_slug
        if self.paper is not None:
            await self.paper.poll()
            opens = await self.paper.list_open_orders(self.tournament_id)
            positions = {ex: h.quantity for ex, h in self.paper.ledger.holdings.items() if h.quantity}
            self.paper.marks.update(marks)
            self.collateral.observe_collateral(self.paper.get_collateral())
            return Portfolio(
                opens, positions, dict(marks), self.paper.account_value(), self.paper.ledger.cash
            )
        opens = await self.read("open_orders", lambda: self.client.list_open_orders(self.tournament_id))
        pos = await self.read("positions", lambda: self.client.get_positions(slug))
        pnl = await self.read("pnl", lambda: self.client.get_pnl(slug, period="all"))
        if self.risk.uses_collateral:
            # Only worth the extra read when the advances feed sizing; order responses keep the
            # outstanding map current in between.
            coll = await self.read("collateral", lambda: self.client.get_collateral(self.tournament_id))
            self.collateral.observe_collateral(coll)
        positions = {p.exchange_id: round(p.quantity) for p in pos.positions if round(p.quantity)}
        pmarks = dict(marks)
        for p in pos.positions:
            if p.current_price is not None:
                pmarks[p.exchange_id] = to_pips(p.current_price)
        # Cash = account value - holdings value. If the engine nets the ALL advance out of account
        # value (undocumented), this under-states cash: the safe direction.
        cash = (
            pnl.total_account_value - pnl.total_holdings_value
            if pnl.total_holdings_value is not None
            else None
        )
        return Portfolio(opens, positions, pmarks, pnl.total_account_value, cash)


@dataclass(frozen=True, slots=True)
class _PendingView:
    """A not-yet-sent bundle leg, counted by risk as if it were resting."""

    request: OrderRequest
    #: Marks a bundle leg that risk's collateral cost model prices itself (never reserved twice).
    pending: bool = True

    @property
    def exchange_id(self) -> str:
        return self.request.exchange_id

    @property
    def yes_dir(self) -> int:
        return self.request.yes_direction

    @property
    def yes_price(self) -> int:
        return self.request.yes_price_pips

    @property
    def remaining(self) -> int:
        return self.request.quantity
