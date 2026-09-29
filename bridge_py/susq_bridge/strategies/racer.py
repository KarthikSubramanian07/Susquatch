"""Constraints racer: trade engine-reported relationship violations as hedged bundles, fast.

Event-driven on ``relationships:violations:{tournamentId}`` (realtime), with a REST polling fallback
while realtime is down. For each violation it builds a bundle and **proves a guaranteed payoff**
before trading: the bundle's payoff is evaluated in every joint outcome the relationship allows, and
the minimum must beat the (worst-price) cost by ``min_edge`` per bundle share.

* mutually_exclusive, sum of YES bids > 1: buy NO on every leg. Outcomes: each single winner,
  plus "none".
* mutually_exclusive, sum of YES asks < 1: buy YES on every leg. "None" is excluded ONLY if the
  relationship is exhaustive AND every leg has a verified MarketRule with ``independent_can_win``
  false; otherwise the proof includes "none" and fails.
* implication / monotonic (A => C), P(A) > P(C): buy C YES + A NO. Outcomes (1,1), (0,1), (0,0).
* complementary (A = not B): NO both when bids sum > 1, YES both when asks sum < 1; outcomes are
  "exactly one YES", plus "none" unless verified as above.
* anything else: log only.

A non-exhaustive set (Nebraska Senate with an independent) can resolve all-NO, so the YES bundle's
minimum payoff is 0 and the proof fails: the racer refuses it. The NO bundle is safe either way.

Legs are priced by walking the current book, sized to the depth that keeps the edge and to
``max_bundle_notional_frac`` of equity, sent atomically via ``/orders/multi-leg`` (never with
``relationshipConstraint``), and any unfilled remainder is cancelled immediately.
"""

from __future__ import annotations

import asyncio
import itertools
import logging
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

from susq.platform.models import ConstraintEvaluation, OrderBook, Relationship
from susq.platform.prices import MAX_PIPS, MIN_PIPS, PIPS_PER_UNIT, to_pips
from susq.platform.realtime import BroadcastMeta

from susq_bridge.config import RacerConfig
from susq_bridge.gateway import BundleOutcome, Gateway, OrderSpec
from susq_bridge.market_state import MarketState

__all__ = [
    "Bundle",
    "Leg",
    "Racer",
    "RacerStats",
    "allowed_outcomes",
    "build_bundle",
    "guaranteed_payoff",
    "ladder",
]

log = logging.getLogger(__name__)
Outcome = dict[str, int]  # exchange id -> 1 (YES) / 0 (NO)


@dataclass(frozen=True, slots=True)
class Leg:
    exchange_id: str
    side: str  # "yes" | "no"
    ladder: tuple[tuple[int, int], ...]  # (side-relative price, qty), cheapest first


@dataclass(frozen=True, slots=True)
class Bundle:
    relationship_id: str
    kind: str
    legs: tuple[Leg, ...]
    quantity: int
    limits: tuple[int, ...]  # side-relative limit per leg (worst level needed)
    cost_pips: int  # per bundle share at the limits
    min_payoff_pips: int
    edge_pips: int


@dataclass
class RacerStats:
    violations: int = 0
    bundles: int = 0
    unsafe: int = 0
    no_edge: int = 0
    log_only: int = 0
    latencies_ms: list[float] = field(default_factory=list)


def ladder(book: OrderBook, side: str) -> tuple[tuple[int, int], ...]:
    """Liquidity for BUYING ``side``: YES buys lift asks; NO buys hit YES bids at ``1 - bid``."""
    if side == "yes":
        levels = [(to_pips(lv.price), round(lv.quantity)) for lv in book.asks]
    else:
        levels = [(PIPS_PER_UNIT - to_pips(lv.price), round(lv.quantity)) for lv in book.bids]
    return tuple(sorted((p, q) for p, q in levels if q > 0))


def allowed_outcomes(
    kind: str, exchange_ids: Sequence[str], *, none_possible: bool, antecedent: str | None = None
) -> list[Outcome]:
    """Joint outcomes the relationship permits (plus "none" when it can't be ruled out)."""
    if kind in ("implication", "monotonic"):
        assert antecedent is not None
        consequent = next(e for e in exchange_ids if e != antecedent)
        return [
            {antecedent: 1, consequent: 1},
            {antecedent: 0, consequent: 1},
            {antecedent: 0, consequent: 0},
        ]
    singles = [{e: int(e == winner) for e in exchange_ids} for winner in exchange_ids]
    return [*singles, {e: 0 for e in exchange_ids}] if none_possible else singles


def guaranteed_payoff(sides: dict[str, str], outcomes: Sequence[Outcome]) -> int:
    """Minimum payoff (pips per bundle share) across ``outcomes``."""
    return min(
        sum(PIPS_PER_UNIT * (outcome[ex] if side == "yes" else 1 - outcome[ex]) for ex, side in sides.items())
        for outcome in outcomes
    )


def _worst_price(lad: Sequence[tuple[int, int]], qty: int) -> int | None:
    filled = 0
    for price, q in lad:
        filled += q
        if filled >= qty:
            return price
    return None


def build_bundle(
    relationship_id: str,
    kind: str,
    legs: Sequence[Leg],
    outcomes: Sequence[Outcome],
    *,
    min_edge_pips: int,
    max_notional_pips: int,
) -> tuple[Bundle | None, str]:
    """Largest equal-quantity bundle whose guaranteed payoff beats its worst-price cost by the edge."""
    payoff = guaranteed_payoff({leg.exchange_id: leg.side for leg in legs}, outcomes)
    if any(not leg.ladder for leg in legs):
        return None, "no_depth"
    breakpoints = sorted({c for leg in legs for c in itertools.accumulate(q for _, q in leg.ladder)})
    best: Bundle | None = None
    for qty in breakpoints:
        limits = [_worst_price(leg.ladder, qty) for leg in legs]
        if any(p is None for p in limits):
            break
        worst = [p for p in limits if p is not None]
        cost = sum(worst)
        if payoff - cost < min_edge_pips or any(not MIN_PIPS <= p <= MAX_PIPS for p in worst):
            break
        size = min(qty, max_notional_pips // cost) if cost else qty
        if size < 1:
            break
        best = Bundle(relationship_id, kind, tuple(legs), size, tuple(worst), cost, payoff, payoff - cost)
        if size < qty:
            break
    if best is None:
        return None, "no_edge"
    return best, "ok"


class Racer:
    def __init__(
        self,
        gateway: Gateway,
        market: MarketState,
        config: RacerConfig,
        *,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
    ) -> None:
        self.gateway = gateway
        self.market = market
        self.config = config
        self.clock = clock
        self.relationships: dict[str, Relationship] = {}
        self.stats = RacerStats()
        self._last_action: dict[str, float] = {}
        self._lock = asyncio.Lock()

    async def load_relationships(self) -> int:
        client = self.gateway.client
        tid = self.market.tournament_id
        rels: dict[str, Relationship] = {}
        cursor: str | None = None
        while True:
            page = await self.gateway.read(
                "relationships", lambda c=cursor: client.list_relationships(tid, cursor=c)
            )
            rels.update({r.id: r for r in page.data})
            if not page.pagination.has_more or not page.pagination.next_cursor:
                break
            cursor = page.pagination.next_cursor
        self.relationships = rels
        return len(rels)

    # ── entry points ────────────────────────────────────────────────────────────────────────

    async def on_violation(self, payload: dict[str, Any], meta: BroadcastMeta) -> None:
        """Realtime callback (``violation`` on ``relationships:violations:{tournamentId}``)."""
        evaluation = ConstraintEvaluation.model_validate(payload)
        await self.handle(evaluation, meta.received_at)

    async def poll_once(self) -> list[BundleOutcome]:
        client = self.gateway.client
        tid = self.market.tournament_id
        res = await self.gateway.read(
            "constraints",
            lambda: client.get_constraints(tid, violations_only=True, min_violation=self.config.min_edge),
        )
        outcomes = []
        for ev in res.data:
            outcome = await self.handle(ev, time.time())
            if outcome is not None:
                outcomes.append(outcome)
        return outcomes

    async def run_polling(self, realtime_up: Callable[[], bool]) -> None:
        """Fallback loop: poll only while realtime is down."""
        while True:
            await asyncio.sleep(self.config.poll_interval_s)
            if realtime_up():
                continue
            try:
                await self.poll_once()
            except Exception:
                log.exception("racer poll failed")

    # ── core ────────────────────────────────────────────────────────────────────────────────

    async def handle(self, ev: ConstraintEvaluation, received_at: float) -> BundleOutcome | None:
        if ev.evaluation_status != "violated":
            return None
        if ev.tournament_id is not None and ev.tournament_id != self.market.tournament_id:
            return None
        self.stats.violations += 1
        async with self._lock:  # one bundle at a time; dedupes bursts of the same violation
            if time.time() - self._last_action.get(ev.relationship_id, 0.0) < self.config.refire_cooldown_s:
                return None
            rel = self.relationships.get(ev.relationship_id)
            if rel is None:
                await self.load_relationships()
                rel = self.relationships.get(ev.relationship_id)
            if rel is None:
                self._journal("racer_skip", ev, reason="unknown_relationship")
                return None
            plan = await self._plan(rel)
            if plan is None:
                return None
            bundle, reason = plan
            if bundle is None:
                if reason == "unsafe":
                    self.stats.unsafe += 1
                else:
                    self.stats.no_edge += 1
                self._journal("racer_skip", ev, reason=reason)
                return None
            self._last_action[ev.relationship_id] = time.time()
            return await self._execute(ev, bundle, received_at)

    async def _plan(self, rel: Relationship) -> tuple[Bundle | None, str] | None:
        kind = rel.type
        if kind in ("implication", "monotonic"):
            direction = rel.direction
            ante = rel.exchange_ids("antecedent") or (direction.from_exchange_ids if direction else [])
            cons = rel.exchange_ids("consequent") or (direction.to_exchange_ids if direction else [])
            if len(ante) != 1 or len(cons) != 1:
                self.stats.log_only += 1
                self._journal_rel("racer_log_only", rel, "unsupported shape")
                return None
            a, c = ante[0], cons[0]
            books = await self._books([a, c])
            legs = [Leg(a, "no", ladder(books[a], "no")), Leg(c, "yes", ladder(books[c], "yes"))]
            outcomes = allowed_outcomes(kind, [a, c], none_possible=True, antecedent=a)
            return self._build(rel, legs, outcomes)
        if kind in ("mutually_exclusive", "complementary"):
            members = rel.exchange_ids("member") or rel.exchange_ids()
            if len(members) < 2 or (kind == "complementary" and len(members) != 2):
                self.stats.log_only += 1
                self._journal_rel("racer_log_only", rel, "unsupported shape")
                return None
            books = await self._books(members)
            bids = [books[m].bids[0].price if books[m].bids else 0.0 for m in members]
            asks = [books[m].asks[0].price if books[m].asks else 1.0 for m in members]
            none_possible = not self._exhaustive_verified(rel, members)
            if sum(bids) > 1:
                side = "no"
            elif sum(asks) < 1:
                side = "yes"
            else:
                return None, "no_edge"
            legs = [Leg(m, side, ladder(books[m], side)) for m in members]
            outcomes = allowed_outcomes(kind, members, none_possible=none_possible)
            bundle, reason = self._build(rel, legs, outcomes)
            if bundle is None and side == "yes" and none_possible:
                # The proof failed because "nobody wins" can't be ruled out (e.g. an independent).
                reason = "unsafe"
            return bundle, reason
        self.stats.log_only += 1
        self._journal_rel("racer_log_only", rel, f"type {kind}")
        return None

    def _exhaustive_verified(self, rel: Relationship, members: Sequence[str]) -> bool:
        """ "None of them wins" is impossible only if the engine says exhaustive AND we verified the rules."""
        if rel.type == "mutually_exclusive" and not rel.is_exhaustive:
            return False
        for ex in members:
            rule = self.market.verified_rule(ex)
            if rule is None or rule.independent_can_win:
                return False
        return True

    def _build(
        self, rel: Relationship, legs: list[Leg], outcomes: list[Outcome]
    ) -> tuple[Bundle | None, str]:
        equity_pips = int(self.gateway.risk.equity * PIPS_PER_UNIT)
        # Per-bundle budget, further capped by what this relationship's legs already hold, so a standing
        # violation can't accumulate unbounded exposure one cooldown at a time.
        held = sum(abs(self.gateway.risk.position_value_pips(leg.exchange_id)) for leg in legs)
        room = max(0, int(self.config.max_relationship_notional_frac * equity_pips) - held)
        max_notional = min(int(self.config.max_bundle_notional_frac * equity_pips), room)
        if max_notional <= 0:
            return None, "relationship_exposure_cap"
        return build_bundle(
            rel.id,
            rel.type,
            legs,
            outcomes,
            min_edge_pips=round(self.config.min_edge * PIPS_PER_UNIT),
            max_notional_pips=max_notional,
        )

    async def _books(self, exchange_ids: Sequence[str]) -> dict[str, OrderBook]:
        books = await asyncio.gather(
            *(self.market.book(ex, max_age_s=0.0, priority=True) for ex in exchange_ids)
        )
        return dict(zip(exchange_ids, books, strict=True))

    async def _execute(self, ev: ConstraintEvaluation, bundle: Bundle, received_at: float) -> BundleOutcome:
        stamp = int(ev.computed_at.timestamp()) if ev.computed_at else int(received_at)
        specs = [
            OrderSpec(
                exchange_id=leg.exchange_id,
                side=leg.side,  # type: ignore[arg-type]
                action="buy",
                quantity=bundle.quantity,
                price_pips=limit,
                ttl_s=self.config.order_ttl_s,
                input_ts=received_at,
            )
            for leg, limit in zip(bundle.legs, bundle.limits, strict=True)
        ]
        outcome = await self.gateway.place_bundle(
            f"racer:{bundle.relationship_id}:{stamp}", "racer", specs, ttl_s=self.config.order_ttl_s
        )
        latency_ms = (time.time() - received_at) * 1000
        if outcome.placed:
            self.stats.bundles += 1
            self.stats.latencies_ms.append(latency_ms)
            for res in outcome.results:
                if res.open and res.order_id is not None:
                    await self.gateway.cancel(res.order_id)
        self.gateway.journal.event(
            "racer_bundle",
            {
                "relationshipId": bundle.relationship_id,
                "type": bundle.kind,
                "status": outcome.status,
                "reason": outcome.reason,
                "quantity": bundle.quantity,
                "legs": [
                    [leg.exchange_id, leg.side, lim]
                    for leg, lim in zip(bundle.legs, bundle.limits, strict=True)
                ],
                "edgePips": bundle.edge_pips,
                "filled": [r.quantity_traded for r in outcome.results],
                "latencyMs": round(latency_ms, 1),
                "violationAmount": ev.violation_amount,
            },
            idempotency_key=outcome.idempotency_key,
        )
        return outcome

    def _journal(self, kind: str, ev: ConstraintEvaluation, *, reason: str) -> None:
        self.gateway.journal.event(
            kind, {"relationshipId": ev.relationship_id, "type": ev.type, "reason": reason}
        )

    def _journal_rel(self, kind: str, rel: Relationship, reason: str) -> None:
        self.gateway.journal.event(kind, {"relationshipId": rel.id, "type": rel.type, "reason": reason})
