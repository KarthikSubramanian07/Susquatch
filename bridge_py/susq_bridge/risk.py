"""Pre-trade risk: caps, breakers, locked legs, tournament and self-cross guards.

Exposure is measured in money (pip-units, 1 SUSQie = 10 000) the conservative way:

* a position is worth ``|qty|`` times its side-relative mark (YES at ``m``, NO at ``1 - m``; an
  unmarked position is valued at the full 1.0 payout);
* every resting order counts its full opening notional (``remaining * side-relative limit``), as if it
  fills;
* a new order counts only the part that *opens* exposure: shares that close an existing position
  (buying the opposite side nets and pays 1) add nothing.

``check`` never returns a quantity that would push any cap past its limit; when a cap binds it
shrinks the order to the largest compliant size instead of rejecting outright.

**Buying power** (only once the portfolio has reported ``cash``): what the order would consume must
fit in cash minus what our resting orders could consume (every resting order at full notional).
By default an order consumes the full notional of the shares it opens (closing shares are free),
exactly the conservative rule the platform applies without collateral. With
``[risk] use_collateral_buying_power = true`` it consumes the *modeled effective cost* instead
(``cost_model``: nominal minus the ALL advance the relationship component's floor would back; see
:mod:`susq_bridge.collateral`), and legs of the same bundle are costed together, in order. Exposure
caps always stay on full notional.
"""

from __future__ import annotations

import math
from collections import deque
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass, field, replace
from datetime import UTC, datetime, timedelta
from typing import Protocol

from susq.platform.models import OrderRequest
from susq.platform.prices import PIPS_PER_UNIT

from susq_bridge.config import RiskConfig

__all__ = ["Decision", "OrderView", "RiskEngine"]


class OrderView(Protocol):
    """What risk needs to know about one of our resting orders."""

    exchange_id: str
    yes_dir: int
    yes_price: int
    remaining: int


@dataclass(frozen=True, slots=True)
class Decision:
    accepted: bool
    quantity: int
    reason: str | None = None
    binding: tuple[str, ...] = ()
    glitch: bool = False
    #: What the caps saw (SUSQies): equity, total exposure, this market's exposure, resting count.
    detail: dict[str, float] = field(default_factory=dict)


def _side_price(yes_dir: int, yes_price: int) -> int:
    return yes_price if yes_dir > 0 else PIPS_PER_UNIT - yes_price


def _largest(limit: int, fits: Callable[[int], bool]) -> int:
    """Largest ``q`` in ``[0, limit]`` with ``fits(q)`` (``fits`` monotone, ``fits(0)`` assumed)."""
    if fits(limit):
        return limit
    lo, hi = 0, limit
    while hi - lo > 1:
        mid = (lo + hi) // 2
        lo, hi = (mid, hi) if fits(mid) else (lo, mid)
    return lo


@dataclass
class RiskEngine:
    config: RiskConfig
    tournament_id: str
    #: exchange id -> cluster names ("national", "state:ME", ...), from MarketRule.clusters
    clusters: Mapping[str, Iterable[str]] = field(default_factory=dict)
    locked: set[str] = field(default_factory=set)
    clock: Callable[[], datetime] = field(default=lambda: datetime.now(UTC))
    equity: float = 0.0
    positions: dict[str, int] = field(default_factory=dict)
    marks: dict[str, int] = field(default_factory=dict)
    frozen_reason: str | None = None
    glitch_used_pips: int = 0
    #: Tournament cash from the last portfolio read (None until known: buying power unchecked).
    cash: float | None = None
    #: Cash (pip-units) a sequence of requests would consume under the ALL collateral model.
    cost_model: Callable[[Sequence[OrderRequest]], int] | None = None
    #: Set at the open when ``collateral_auto`` is on and measure_day1 confirmed the model.
    collateral_confirmed: bool = False
    _rejects: deque[datetime] = field(default_factory=deque)
    _equity: deque[tuple[datetime, float]] = field(default_factory=deque)
    on_trip: Callable[[str], None] | None = None

    # ── state ───────────────────────────────────────────────────────────────────────────────

    @property
    def frozen(self) -> bool:
        return self.frozen_reason is not None

    def update_portfolio(
        self,
        positions: Mapping[str, int],
        marks: Mapping[str, int],
        equity: float,
        cash: float | None = None,
    ) -> None:
        self.positions = {ex: q for ex, q in positions.items() if q}
        self.marks.update(marks)
        self.equity = equity
        if cash is not None:
            self.cash = cash
        self.observe_equity(equity)

    def trip(self, reason: str) -> None:
        if self.frozen_reason is None:
            self.frozen_reason = reason
            if self.on_trip is not None:
                self.on_trip(reason)

    def record_reject(self) -> None:
        now = self.clock()
        self._rejects.append(now)
        while self._rejects and now - self._rejects[0] > timedelta(seconds=60):
            self._rejects.popleft()
        if len(self._rejects) > self.config.reject_breaker_per_min:
            self.trip(f"reject_breaker: {len(self._rejects)} rejects in 60s")

    def observe_equity(self, equity: float) -> None:
        """Loss-per-hour breaker on mark-to-market equity."""
        now = self.clock()
        self._equity.append((now, equity))
        while self._equity and now - self._equity[0][0] > timedelta(hours=1):
            self._equity.popleft()
        peak = max(e for _, e in self._equity)
        if peak > 0 and (peak - equity) >= self.config.loss_per_hour_frac * peak:
            self.trip(f"loss_per_hour: equity {equity:.2f} vs 1h peak {peak:.2f}")

    # ── exposure ────────────────────────────────────────────────────────────────────────────

    def position_value_pips(self, exchange_id: str) -> int:
        qty = self.positions.get(exchange_id, 0)
        if not qty:
            return 0
        mark = self.marks.get(exchange_id)
        if mark is None:
            return abs(qty) * PIPS_PER_UNIT
        return abs(qty) * _side_price(1 if qty > 0 else -1, mark)

    def total_exposure_pips(self, open_orders: Iterable[OrderView]) -> int:
        """Positions at mark plus resting orders, across every market (what ``total_gross`` caps)."""
        orders = list(open_orders)
        exchanges = set(self.positions) | {o.exchange_id for o in orders}
        return sum(self.market_exposure_pips(e, orders) for e in exchanges)

    def market_exposure_pips(self, exchange_id: str, open_orders: Iterable[OrderView]) -> int:
        resting = sum(
            o.remaining * _side_price(o.yes_dir, o.yes_price)
            for o in open_orders
            if o.exchange_id == exchange_id
        )
        return self.position_value_pips(exchange_id) + resting

    def clusters_of(self, exchange_id: str) -> list[str]:
        return list(self.clusters.get(exchange_id, ()))

    def cluster_cap_frac(self, cluster: str) -> float | None:
        return self.config.clusters.get(cluster.split(":", 1)[0])

    def closing_shares(self, order: OrderRequest) -> int:
        pos = self.positions.get(order.exchange_id, 0)
        return min(order.quantity, abs(pos)) if pos * order.yes_direction < 0 else 0

    # ── the check ───────────────────────────────────────────────────────────────────────────

    def check(
        self,
        order: OrderRequest,
        open_orders: Iterable[OrderView],
        *,
        fair_pips: int | None = None,
        max_notional: float | None = None,
        prior_legs: Sequence[OrderRequest] = (),
    ) -> Decision:
        """Accept (possibly smaller) or reject ``order``. Never mutates state (see :meth:`commit`).

        ``prior_legs`` are the earlier legs of the same atomic bundle (already in ``open_orders`` as
        pending views); the collateral cost model prices this leg after them.
        """
        orders = list(open_orders)
        if order.tournament_id != self.tournament_id:
            return Decision(False, 0, "tournament_mismatch")
        if self.frozen:
            return Decision(False, 0, f"frozen: {self.frozen_reason}")
        if self.equity <= 0:
            return Decision(False, 0, "no_equity")
        ex = order.exchange_id
        pos = self.positions.get(ex, 0)
        if ex in self.locked and pos * order.yes_direction < 0:
            return Decision(False, 0, "locked_leg")
        for o in orders:
            if o.exchange_id != ex or o.yes_dir == order.yes_direction:
                continue
            crosses = (
                o.yes_price <= order.yes_price_pips
                if order.yes_direction > 0
                else o.yes_price >= order.yes_price_pips
            )
            if crosses:
                return Decision(False, 0, "self_cross")

        unit = _side_price(order.yes_direction, order.yes_price_pips)
        closing = self.closing_shares(order)
        equity_pips = self.equity * PIPS_PER_UNIT
        limits: dict[str, int] = {"requested": order.quantity}

        def room(cap_pips: float, used_pips: int) -> int:
            return closing + max(0, math.floor((cap_pips - used_pips) / unit)) if unit else order.quantity

        per_order = self.config.max_order_notional_frac * equity_pips
        if max_notional is not None:
            per_order = min(per_order, max_notional * PIPS_PER_UNIT)
        limits["per_order"] = room(per_order, 0)
        limits["per_market"] = room(
            self.config.per_market_gross_frac * equity_pips, self.market_exposure_pips(ex, orders)
        )
        exchanges = set(self.positions) | {o.exchange_id for o in orders} | {ex}
        total = sum(self.market_exposure_pips(e, orders) for e in exchanges)
        limits["total_gross"] = room(self.config.total_gross_frac * equity_pips, total)
        for cluster in self.clusters_of(ex):
            frac = self.cluster_cap_frac(cluster)
            if frac is None:
                continue
            members = [e for e in exchanges if cluster in self.clusters_of(e)]
            used = sum(self.market_exposure_pips(e, orders) for e in members)
            limits[f"cluster:{cluster}"] = room(frac * equity_pips, used)

        if self.cash is not None:
            limits["buying_power"] = self.buying_power_room(order, orders, prior_legs, closing, unit)

        glitch = False
        if fair_pips is not None:
            through = (fair_pips - order.yes_price_pips) * order.yes_direction
            if through > self.config.glitch_through_fair_pts * 100:
                glitch = True
                limits["glitch"] = room(self.config.glitch_max_frac * equity_pips, self.glitch_used_pips)

        qty = min(limits.values())
        binding = tuple(k for k, v in limits.items() if v == qty and k != "requested")
        detail = {
            "equity": round(self.equity, 2),
            "total": round(total / PIPS_PER_UNIT, 2),
            "market": round(self.market_exposure_pips(ex, orders) / PIPS_PER_UNIT, 2),
            "positions": float(len(self.positions)),
            "resting": float(len(orders)),
        }
        if qty < 1:
            return Decision(False, 0, f"cap: {', '.join(binding)}", binding, glitch, detail)
        return Decision(True, qty, None if qty == order.quantity else "reduced", binding, glitch, detail)

    @property
    def uses_collateral(self) -> bool:
        enabled = self.config.use_collateral_buying_power or (
            self.config.collateral_auto and self.collateral_confirmed
        )
        return enabled and self.cost_model is not None

    def buying_power_room(
        self,
        order: OrderRequest,
        orders: Sequence[OrderView],
        prior_legs: Sequence[OrderRequest],
        closing: int,
        unit: int,
    ) -> int:
        """Largest quantity whose cash consumption fits in cash minus resting-order reservations."""
        assert self.cash is not None
        collateral = self.uses_collateral
        reserved = sum(
            o.remaining * _side_price(o.yes_dir, o.yes_price)
            for o in orders
            # Pending legs of this bundle are costed by the model below, not reserved twice.
            if not (collateral and getattr(o, "pending", False))
        )
        # Over-committed or not, shares that cost nothing (closing a position) always fit.
        available = max(0, math.floor(self.cash * PIPS_PER_UNIT) - reserved)
        if not collateral:
            return closing + available // unit if unit else order.quantity
        assert self.cost_model is not None
        model = self.cost_model
        base = model(prior_legs)
        return _largest(
            order.quantity, lambda q: model([*prior_legs, replace(order, quantity=q)]) - base <= available
        )

    def commit(self, order: OrderRequest, decision: Decision) -> None:
        """Book an accepted order's glitch budget (exposure itself is tracked through open orders)."""
        if decision.accepted and decision.glitch:
            opened = max(0, decision.quantity - self.closing_shares(order))
            self.glitch_used_pips += opened * _side_price(order.yes_direction, order.yes_price_pips)

    def snapshot(self) -> dict[str, object]:
        return {
            "equity": self.equity,
            "cash": self.cash,
            "collateralBuyingPower": self.uses_collateral,
            "frozen": self.frozen_reason,
            "glitch_used": self.glitch_used_pips / PIPS_PER_UNIT,
            "positions": dict(self.positions),
        }
