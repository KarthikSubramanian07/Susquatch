"""Stale-order sniper: take resting orders that sit well through fair value.

A student bid above fair (or ask below fair) left behind after the real-money market moved is free
edge, *if* fair is right. So the sniper only ever acts when:

* the market has a **verified** MarketRule (a human has checked the Cup <-> reference mapping);
* the fair value is **fresh** (``fair.stale_after_s``) and **sane** (Cup mid within
  ``fair.mapping_error_pts`` of fair; otherwise it's probably an inverted or wrong mapping);
* the resting level is at least ``min_edge_pts`` through fair on the tick grid.

It takes exactly the stale levels (limit = the worst level still ``min_edge_pts`` through fair), never
walks into fair-priced liquidity, and sizes so that if fair were completely wrong the fill would lose
less than ``max_mistake_frac`` of bankroll. Per-market/cluster caps and the glitch cap are enforced by
the gateway's risk check; orders carry ``order_ttl_s`` and are cancelled when their fair input goes stale.

**Unwind (EXP-003).** From ``unwind_from_utc`` the sniper buys nothing and instead sells its book to
free cash for the scenario book and the election-night budget. Positions are account-level and the
platform does not say who bought what, so the *unwind set* is defined by a rule both traders apply
identically (rust-live: ``crates/strategies/src/sniper.rs``). A held side is in the set when

* the market is verified and its fair is fresh and sane (the same gate as a snipe);
* the held side's fair is at least 0.90 (YES held and fair >= 0.90, or NO held and fair <= 0.10);
* the exchange is not a locked leg in the current targets file;
* no other strategy (anything but ``sniper``, including orders of unknown origin) has an order resting
  there, and the sniper itself has none working there.

Each sell is covered (at most the holding, so it never flips short) and priced at
``max(held-side bid, ceil_tick(side fair - unwind_max_haircut_pts))``, never below that floor. It is
sized by the existing per-order rule (proceeds at most ``max_mistake_frac`` of equity), by the cash
still missing to ``unwind_target_cash_frac`` of equity, and by ``unwind_max_hourly_frac`` of equity of
proceeds per trailing hour. The unwind stops when cash reaches the target, and waits while cash is
unknown or the targets file cannot be read (it could not tell locked legs from the rest).
"""

from __future__ import annotations

import math
import time
from collections import deque
from collections.abc import Callable, Iterable, Mapping
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path

from susq.calibrate.targets import read_current
from susq.platform.prices import PIPS_PER_UNIT, to_pips

from susq_bridge.config import SniperConfig
from susq_bridge.gateway import Gateway, OrderOutcome, OrderSpec
from susq_bridge.market_state import MarketState

__all__ = [
    "UNWIND_MIN_SIDE_FAIR_PIPS",
    "Sniper",
    "SniperStats",
    "Target",
    "UnwindOrder",
    "UnwindPlan",
    "plan_unwind",
]

#: Strategy name on orders and intents.
NAME = "sniper"
#: A held side is in the unwind set only when its fair is at least 0.90 (a near-certain outcome).
UNWIND_MIN_SIDE_FAIR_PIPS = 9_000
TICK_PIPS = 50
MAX_PRICE_PIPS = PIPS_PER_UNIT - TICK_PIPS  # 0.995


@dataclass(frozen=True, slots=True)
class Target:
    exchange_id: str
    side: str  # what we buy: "yes" lifts stale asks, "no" hits stale bids
    limit_pips: int  # side-relative
    quantity: int
    fair_pips: int
    edge_pips: int


@dataclass(frozen=True, slots=True)
class UnwindOrder:
    """A covered sell of a held side (all prices side-relative pips)."""

    exchange_id: str
    side: str  # the held side being sold: "yes" or "no"
    quantity: int
    price_pips: int
    fair_pips: int  # the held side's fair
    floor_pips: int  # never below this: ceil_tick(fair - haircut)
    bid_pips: int | None  # the held side's best bid when planned


@dataclass
class UnwindPlan:
    orders: list[UnwindOrder] = field(default_factory=list)
    #: exchange id -> why a held exchange was left alone this pass.
    skipped: dict[str, str] = field(default_factory=dict)
    #: Why the pass stopped early: not_started, no_cash, cash_target, hourly_cap, targets_unreadable.
    stop: str | None = None


@dataclass
class SniperStats:
    scans: int = 0
    candidates: int = 0
    orders: int = 0
    blocked: dict[str, int] = field(default_factory=dict)


def _ceil_tick(pips: int) -> int:
    return -(-pips // TICK_PIPS) * TICK_PIPS


def plan_unwind(
    config: SniperConfig,
    *,
    now: datetime,
    equity: float,
    cash: float | None,
    hourly_used: float,
    positions: Mapping[str, int],
    fair_pips: Mapping[str, int],
    tops: Mapping[str, tuple[int | None, int | None]],
    locked: Iterable[str] = (),
    other_strategy: Iterable[str] = (),
    busy: Iterable[str] = (),
) -> UnwindPlan:
    """The unwind sells for one pass. Pure: ``tests/fixtures/sniper_unwind.json`` holds Rust to the same rule.

    ``fair_pips`` holds YES fairs only for exchanges that passed the fair gate (verified, fresh, sane);
    ``tops`` are YES-space ``(best bid, best ask)`` pips; ``hourly_used`` is SUSQies of unwind proceeds
    sent in the trailing hour; ``other_strategy`` and ``busy`` are exchanges with resting orders of
    another strategy and of the sniper.
    """
    plan = UnwindPlan()
    start = config.unwind_from_utc
    if start is None or now < start:
        plan.stop = "not_started"
        return plan
    if cash is None or equity <= 0:
        plan.stop = "no_cash"
        return plan
    gap = (config.unwind_target_cash_frac * equity - cash) * PIPS_PER_UNIT
    if gap <= 0:
        plan.stop = "cash_target"
        return plan
    hour_room = (config.unwind_max_hourly_frac * equity - hourly_used) * PIPS_PER_UNIT
    per_order = config.max_mistake_frac * equity * PIPS_PER_UNIT
    haircut = round(config.unwind_max_haircut_pts * 100)
    locked_set, other_set, busy_set = set(locked), set(other_strategy), set(busy)
    for ex in sorted(positions):
        qty = positions[ex]
        if not qty:
            continue
        fair = fair_pips.get(ex)
        if fair is None:
            plan.skipped[ex] = "no_fair"
            continue
        side = "yes" if qty > 0 else "no"
        side_fair = fair if side == "yes" else PIPS_PER_UNIT - fair
        if side_fair < UNWIND_MIN_SIDE_FAIR_PIPS:
            plan.skipped[ex] = "not_near_certain"
            continue
        if ex in locked_set:
            plan.skipped[ex] = "locked"
            continue
        if ex in other_set:
            plan.skipped[ex] = "other_strategy"
            continue
        if ex in busy_set:
            plan.skipped[ex] = "busy"
            continue
        floor = _ceil_tick(side_fair - haircut)
        yes_bid, yes_ask = tops.get(ex, (None, None))
        bid = yes_bid if side == "yes" else (PIPS_PER_UNIT - yes_ask if yes_ask is not None else None)
        limit = max(bid, floor) if bid is not None else floor
        if limit > MAX_PRICE_PIPS:  # a fair within the haircut of 1.0 leaves no valid price at the floor
            plan.skipped[ex] = "no_valid_price"
            continue
        if hour_room < limit:
            plan.stop = "hourly_cap"
            break
        size = min(
            abs(qty),
            math.floor(per_order / limit + 1e-9),
            math.ceil(gap / limit - 1e-9),
            math.floor(hour_room / limit + 1e-9),
        )
        if size < 1:
            plan.skipped[ex] = "size"
            continue
        plan.orders.append(UnwindOrder(ex, side, size, limit, side_fair, floor, bid))
        gap -= size * limit
        hour_room -= size * limit
        if gap <= 0:
            plan.stop = "cash_target"
            break
    return plan


class Sniper:
    def __init__(
        self,
        gateway: Gateway,
        market: MarketState,
        config: SniperConfig,
        *,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
        targets_dir: Path | None = None,
    ) -> None:
        self.gateway = gateway
        self.market = market
        self.config = config
        self.clock = clock
        self.targets_dir = targets_dir
        self.stats = SniperStats()
        self._last_action: dict[str, float] = {}
        #: (when, proceeds in SUSQies) of unwind sells sent in the trailing hour.
        self._unwind_sent: deque[tuple[datetime, float]] = deque()
        self._locked: set[str] | None = None
        self._unwind_stop: str | None = None

    def _block(self, reason: str) -> None:
        self.stats.blocked[reason] = self.stats.blocked.get(reason, 0) + 1

    async def find(self, exchange_id: str) -> Target | None:
        """A snipe on ``exchange_id``, or None (with the reason counted in ``stats.blocked``)."""
        check = self.market.fair_check(exchange_id)
        if not check.ok or check.fair_pips is None:
            self._block(check.reason)
            return None
        fair = check.fair_pips
        edge = round(self.config.min_edge_pts * 100)
        snap = self.market.prices.get(exchange_id)
        if snap is None:
            return None
        stale_ask = snap.best_ask is not None and to_pips(snap.best_ask) <= fair - edge
        stale_bid = snap.best_bid is not None and to_pips(snap.best_bid) >= fair + edge
        if not (stale_ask or stale_bid):
            return None
        book = await self.market.book(exchange_id, max_age_s=0.0)
        if stale_ask:
            levels = [
                (to_pips(lv.price), round(lv.quantity))
                for lv in book.asks
                if to_pips(lv.price) <= fair - edge
            ]
            side = "yes"
            limit = max((p for p, _ in levels), default=None)
        else:
            levels = [
                (to_pips(lv.price), round(lv.quantity))
                for lv in book.bids
                if to_pips(lv.price) >= fair + edge
            ]
            side = "no"
            worst_bid = min((p for p, _ in levels), default=None)
            limit = PIPS_PER_UNIT - worst_bid if worst_bid is not None else None
        depth = sum(q for _, q in levels)
        if limit is None or depth <= 0:
            return None  # the snapshot was already stale; the book has moved on
        edge_pips = (fair - limit) if side == "yes" else (limit - (PIPS_PER_UNIT - fair)) * -1
        # A wrong fair costs at most the price paid per share; keep that under max_mistake_frac.
        budget = self.config.max_mistake_frac * self.gateway.risk.equity * PIPS_PER_UNIT
        qty = min(depth, int(budget // limit))
        if qty < 1:
            self._block("size")
            return None
        return Target(exchange_id, side, limit, qty, fair, edge_pips)

    def unwinding(self) -> bool:
        """True from ``unwind_from_utc`` on: no more buys, only unwind sells."""
        start = self.config.unwind_from_utc
        return start is not None and self.clock() >= start

    async def scan(self) -> list[OrderOutcome]:
        """One pass over every verified market with a fresh fair value."""
        self.stats.scans += 1
        self.market.load_fair()
        await self.market.refresh_prices()
        if self.unwinding():
            return await self.unwind()
        outcomes: list[OrderOutcome] = []
        if self.gateway.oms is not None:
            # The sniper's edge is a few points on near-certain outcomes held to settlement. Left
            # alone it fills every cap and starves the strategies that can actually win (far quotes,
            # the scenario book), so it stops adding once the book is this full.
            risk = self.gateway.risk
            total = risk.total_exposure_pips(self.gateway.oms.open_orders())
            if total >= self.config.max_total_gross_frac * risk.equity * PIPS_PER_UNIT:
                self._block("gross_budget")
                return []
        busy = (
            {o.exchange_id for o in self.gateway.oms.open_orders(strategy=NAME)}
            if self.gateway.oms
            else set()
        )
        for exchange_id in sorted(self.market.rules):
            if exchange_id in busy:
                continue
            if time.time() - self._last_action.get(exchange_id, 0.0) < self.config.order_ttl_s:
                continue
            target = await self.find(exchange_id)
            if target is None:
                continue
            self.stats.candidates += 1
            outcome = await self.place(target)
            outcomes.append(outcome)
        return outcomes

    async def place(self, target: Target) -> OrderOutcome:
        fair_ts = self.market.fair_produced_at.timestamp() if self.market.fair_produced_at else None
        self._last_action[target.exchange_id] = time.time()
        spec = OrderSpec(
            exchange_id=target.exchange_id,
            side=target.side,  # type: ignore[arg-type]
            action="buy",
            quantity=target.quantity,
            price_pips=target.limit_pips,
            ttl_s=self.config.order_ttl_s,
            fair_pips=target.fair_pips,
            input_ts=fair_ts,
        )
        stamp = int(self.clock().timestamp())
        outcome = await self.gateway.place(f"sniper:{target.exchange_id}:{stamp}", NAME, spec)
        if outcome.placed:
            self.stats.orders += 1
        self.gateway.journal.event(
            "sniper_order",
            {
                "exchangeId": target.exchange_id,
                "side": target.side,
                "limit": target.limit_pips,
                "quantity": target.quantity,
                "fair": target.fair_pips,
                "edgePips": target.edge_pips,
                "status": outcome.status,
                "reason": outcome.reason,
            },
            idempotency_key=outcome.idempotency_key,
        )
        return outcome

    # ── EXP-003 unwind ──────────────────────────────────────────────────────────────────────

    def locked_legs(self) -> set[str] | None:
        """Exchanges whose targets leg is locked; None when the targets file cannot be read and no
        earlier read succeeded (the unwind then waits rather than risk selling a locked leg)."""
        if self.targets_dir is None:
            return set()
        try:
            tf = read_current(self.targets_dir)
        except Exception:  # half-written or invalid: keep the last good set
            return self._locked
        self._locked = {p.exchange_id for p in tf.positions if p.locked} if tf is not None else set()
        return self._locked

    def hourly_used(self, now: datetime) -> float:
        """SUSQies of unwind proceeds sent in the hour before ``now``."""
        while self._unwind_sent and now - self._unwind_sent[0][0] >= timedelta(hours=1):
            self._unwind_sent.popleft()
        return sum(n for _, n in self._unwind_sent)

    def plan(self) -> UnwindPlan:
        """This pass's unwind sells from the bridge's current view (no platform I/O)."""
        now = self.clock()
        risk = self.gateway.risk
        positions = {ex: q for ex, q in risk.positions.items() if q}
        fair: dict[str, int] = {}
        tops: dict[str, tuple[int | None, int | None]] = {}
        gated: dict[str, str] = {}
        for ex in positions:
            check = self.market.fair_check(ex)
            if not check.ok or check.fair_pips is None:
                gated[ex] = check.reason
                continue
            fair[ex] = check.fair_pips
            snap = self.market.prices.get(ex)
            if snap is not None:
                tops[ex] = (
                    to_pips(snap.best_bid) if snap.best_bid is not None else None,
                    to_pips(snap.best_ask) if snap.best_ask is not None else None,
                )
        locked = self.locked_legs()
        if locked is None:
            return UnwindPlan(stop="targets_unreadable")
        orders = self.gateway.oms.open_orders() if self.gateway.oms is not None else []
        other = {o.exchange_id for o in orders if o.strategy != NAME}
        wall = time.time()
        busy = {o.exchange_id for o in orders if o.strategy == NAME} | {
            ex for ex, t in self._last_action.items() if wall - t < self.config.order_ttl_s
        }
        plan = plan_unwind(
            self.config,
            now=now,
            equity=risk.equity,
            cash=risk.cash,
            hourly_used=self.hourly_used(now),
            positions=positions,
            fair_pips=fair,
            tops=tops,
            locked=locked,
            other_strategy=other,
            busy=busy,
        )
        plan.skipped.update(gated)
        return plan

    async def unwind(self) -> list[OrderOutcome]:
        """One unwind pass: post the planned covered sells and journal each decision."""
        plan = self.plan()
        for reason in plan.skipped.values():
            self._block(f"unwind_{reason}")
        if plan.stop is not None:
            self._block(f"unwind_{plan.stop}")
        if plan.stop != self._unwind_stop:
            # A stop that holds pass after pass is journaled once, when it starts and when it ends.
            self._unwind_stop = plan.stop
            risk = self.gateway.risk
            self.gateway.journal.event(
                "sniper_unwind_state",
                {
                    "stop": plan.stop,
                    "cash": risk.cash,
                    "equity": risk.equity,
                    "targetCash": round(self.config.unwind_target_cash_frac * risk.equity, 2),
                    "hourlyUsed": round(self.hourly_used(self.clock()), 2),
                },
            )
        return [await self.place_unwind(order) for order in plan.orders]

    async def place_unwind(self, order: UnwindOrder) -> OrderOutcome:
        fair_ts = self.market.fair_produced_at.timestamp() if self.market.fair_produced_at else None
        self._last_action[order.exchange_id] = time.time()
        yes_fair = order.fair_pips if order.side == "yes" else PIPS_PER_UNIT - order.fair_pips
        spec = OrderSpec(
            exchange_id=order.exchange_id,
            side=order.side,  # type: ignore[arg-type]
            action="sell",
            quantity=order.quantity,
            price_pips=order.price_pips,
            ttl_s=self.config.order_ttl_s,
            fair_pips=yes_fair,
            input_ts=fair_ts,
        )
        now = self.clock()
        stamp = int(now.timestamp())
        outcome = await self.gateway.place(f"sniper:{order.exchange_id}:unwind:{stamp}", NAME, spec)
        if outcome.status not in ("risk_rejected", "blocked", "duplicate"):
            # Count what went to the platform (filled or resting), as rust-live counts what it emits.
            qty = outcome.request.quantity if outcome.request is not None else order.quantity
            self._unwind_sent.append((now, qty * order.price_pips / PIPS_PER_UNIT))
        if outcome.placed:
            self.stats.orders += 1
        self.gateway.journal.event(
            "sniper_unwind",
            {
                "exchangeId": order.exchange_id,
                "side": order.side,
                "quantity": order.quantity,
                "limit": order.price_pips,
                "fair": order.fair_pips,
                "floor": order.floor_pips,
                "bid": order.bid_pips,
                "haircutPips": order.fair_pips - order.price_pips,
                "status": outcome.status,
                "reason": outcome.reason,
            },
            idempotency_key=outcome.idempotency_key,
        )
        return outcome
