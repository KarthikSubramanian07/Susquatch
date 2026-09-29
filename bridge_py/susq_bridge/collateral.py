"""ALL collateral in the bridge: record what the engine reports, estimate what it would charge.

Two jobs, deliberately separate:

* **observe** (always on): every order response's ``all`` block is folded into per-component
  outstanding advances and kept, next to what the model predicted for the same fill, for the
  day-one comparison; ``GET /portfolio/collateral`` reads replace the outstanding map wholesale.
  The gateway journals each one as ``all_economics``. Positions are shadowed between reconciles
  (:meth:`CollateralTracker.sync_positions`) so each leg of a bundle is modeled after the previous.
* **estimate** (used only when ``[risk] use_collateral_buying_power = true``):
  :meth:`CollateralTracker.effective_cost_pips` runs the shared model
  (:mod:`susq.platform.collateral`, the same code as the mock and the paper broker) over our current
  positions and the last known advances, and returns the cash a sequence of fills would consume.

Until day-one evidence confirms the live engine matches the model, sizing stays on full notional.
"""

from __future__ import annotations

from collections import deque
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass

from susq.platform.collateral import CollateralGraph, FillEconomics, Relation, fill_economics
from susq.platform.models import Collateral, OrderRequest, PlaceOrderResult, Relationship
from susq.platform.prices import PIPS_PER_UNIT

__all__ = ["COMPARED_FIELDS", "CollateralTracker", "FillLeg", "ObservedEconomics", "request_legs"]

#: Fields compared between the engine's ``all`` and the model (SUSQies).
COMPARED_FIELDS = (
    "fullNotionalCost",
    "collateralSavings",
    "effectiveEntryCost",
    "collateralRepayment",
    "outstandingAdvanceAfter",
    "guaranteedPayoutFloorAfter",
)

#: (exchange id, YES direction +1/-1, YES price pips, shares)
FillLeg = tuple[str, int, int, int]


def request_legs(requests: Iterable[OrderRequest]) -> list[FillLeg]:
    """Order requests as model fills at their limits (the worst case the engine could charge)."""
    return [(r.exchange_id, r.yes_direction, r.yes_price_pips, r.quantity) for r in requests]


def _pips(value: float | None) -> int:
    return round((value or 0.0) * PIPS_PER_UNIT)


@dataclass(frozen=True, slots=True)
class ObservedEconomics:
    """One non-null ``all`` block from a real (or paper) order response."""

    intent_id: str
    exchange_id: str
    side: str
    quantity_traded: float
    fill_price: float | None
    full_notional_cost: float
    effective_entry_cost: float
    collateral_savings: float
    collateral_repayment: float
    net_buying_power_impact: float
    component_id: str | None
    relationship_ids: tuple[str, ...]
    guaranteed_payout_floor_after: float
    outstanding_advance_after: float
    #: What the model predicted for this fill (None: the exchange is outside every known relationship)
    modeled: FillEconomics | None = None

    def observed_fields(self) -> dict[str, float]:
        return {
            "fullNotionalCost": self.full_notional_cost,
            "collateralSavings": self.collateral_savings,
            "effectiveEntryCost": self.effective_entry_cost,
            "collateralRepayment": self.collateral_repayment,
            "outstandingAdvanceAfter": self.outstanding_advance_after,
            "guaranteedPayoutFloorAfter": self.guaranteed_payout_floor_after,
        }

    def modeled_fields(self) -> dict[str, float] | None:
        m = self.modeled
        if m is None:
            return None
        return {
            "fullNotionalCost": m.full_cost_pips / PIPS_PER_UNIT,
            "collateralSavings": m.savings_pips / PIPS_PER_UNIT,
            "effectiveEntryCost": m.effective_cost_pips / PIPS_PER_UNIT,
            "collateralRepayment": m.repayment_pips / PIPS_PER_UNIT,
            "outstandingAdvanceAfter": m.outstanding_after_pips / PIPS_PER_UNIT,
            "guaranteedPayoutFloorAfter": (m.floor_after_pips or 0) / PIPS_PER_UNIT,
        }

    def mismatches(self, tolerance: float) -> list[str]:
        """Human-readable differences beyond ``tolerance`` (SUSQies) between engine and model."""
        modeled = self.modeled_fields()
        if modeled is None:
            return [
                f"{self.exchange_id}: engine linked it to {self.component_id}, "
                "the model knows no relationship"
            ]
        observed = self.observed_fields()
        return [
            f"{self.exchange_id} {name}: engine {observed[name]:.4f} vs model {modeled[name]:.4f}"
            for name in COMPARED_FIELDS
            if abs(observed[name] - modeled[name]) > tolerance
        ]

    def journal(self) -> dict[str, object]:
        return {
            "intent": self.intent_id,
            "exchangeId": self.exchange_id,
            "side": self.side,
            "traded": self.quantity_traded,
            "fillPrice": self.fill_price,
            "fullNotionalCost": self.full_notional_cost,
            "effectiveEntryCost": self.effective_entry_cost,
            "collateralSavings": self.collateral_savings,
            "collateralRepayment": self.collateral_repayment,
            "netBuyingPowerImpact": self.net_buying_power_impact,
            "componentId": self.component_id,
            "relationshipIds": list(self.relationship_ids),
            "guaranteedPayoutFloorAfter": self.guaranteed_payout_floor_after,
            "outstandingAdvanceAfter": self.outstanding_advance_after,
            "modeled": self.modeled_fields(),
        }


def _yes_fill(request: OrderRequest, result: PlaceOrderResult) -> tuple[int, int] | None:
    """(YES direction, YES fill price pips) of a response, from its canonical side/action."""
    if not result.quantity_traded or result.fill_price is None:
        return None
    side = result.side or request.side
    action = result.action or request.action
    yes_dir = 1 if (side == "yes") == (action == "buy") else -1
    side_price = _pips(result.fill_price)
    return yes_dir, side_price if side == "yes" else PIPS_PER_UNIT - side_price


class CollateralTracker:
    def __init__(self, *, keep: int = 500) -> None:
        self.graph: CollateralGraph | None = None
        self.relations: list[Relation] = []
        #: component id -> outstanding advance (pip-units), as last reported by the engine
        self.outstanding: dict[str, int] = {}
        #: Our positions as of the last reconcile plus every fill observed since.
        self.positions: dict[str, int] = {}
        self.observed: deque[ObservedEconomics] = deque(maxlen=keep)
        self.responses = 0
        self.with_all = 0
        #: Fills the model links to a relationship that came back with ``all: null``.
        self.linked_without_all = 0
        self.savings_pips = 0

    # ── relationships ───────────────────────────────────────────────────────────────────────

    def set_relationships(self, relationships: Iterable[Relationship]) -> list[Relation]:
        """Rebuild the model graph from ``GET /relationships`` (active ones only)."""
        self.relations = Relation.from_models(relationships)
        self.graph = CollateralGraph(self.relations)
        return self.relations

    # ── observe ─────────────────────────────────────────────────────────────────────────────

    def sync_positions(self, positions: Mapping[str, int]) -> None:
        """Authoritative positions from a reconcile (the shadow restarts from here)."""
        self.positions = {ex: q for ex, q in positions.items() if q}

    def observe(
        self, intent_id: str, request: OrderRequest, result: PlaceOrderResult
    ) -> ObservedEconomics | None:
        """Fold one order response in. Returns the economics if the engine produced a quote."""
        self.responses += 1
        fill = _yes_fill(request, result)
        modeled = None
        if fill is not None:
            yes_qty = fill[0] * round(result.quantity_traded)
            modeled = fill_economics(
                self.graph, self.positions, self.outstanding, request.exchange_id, yes_qty, fill[1]
            )
            self.positions[request.exchange_id] = self.positions.get(request.exchange_id, 0) + yes_qty
            if modeled.component is None:
                modeled = None
        econ = result.all
        if econ is None:
            if modeled is not None:  # the model links this fill, the engine produced no quote
                self.linked_without_all += 1
            return None
        self.with_all += 1
        self.savings_pips += _pips(econ.collateral_savings)
        if econ.component_id is not None and econ.outstanding_advance_after is not None:
            self._set(econ.component_id, _pips(econ.outstanding_advance_after))
        seen = ObservedEconomics(
            intent_id=intent_id,
            exchange_id=request.exchange_id,
            side=result.side or request.side,
            quantity_traded=result.quantity_traded,
            fill_price=result.fill_price,
            full_notional_cost=econ.full_notional_cost or 0.0,
            effective_entry_cost=econ.effective_entry_cost or 0.0,
            collateral_savings=econ.collateral_savings or 0.0,
            collateral_repayment=econ.collateral_repayment or 0.0,
            net_buying_power_impact=econ.net_buying_power_impact or 0.0,
            component_id=econ.component_id,
            relationship_ids=tuple(econ.relationship_ids),
            guaranteed_payout_floor_after=econ.guaranteed_payout_floor_after or 0.0,
            outstanding_advance_after=econ.outstanding_advance_after or 0.0,
            modeled=modeled,
        )
        self.observed.append(seen)
        return seen

    def observe_collateral(self, collateral: Collateral) -> None:
        """Authoritative outstanding advances from ``GET /portfolio/collateral``."""
        self.outstanding = {
            c.component_id: _pips(c.outstanding_advance) for c in collateral.data if c.outstanding_advance
        }

    def _set(self, component_id: str, value: int) -> None:
        if value:
            self.outstanding[component_id] = value
        else:
            self.outstanding.pop(component_id, None)

    # ── estimate ────────────────────────────────────────────────────────────────────────────

    def effective_cost_pips(self, positions: Mapping[str, int], legs: Sequence[FillLeg]) -> int:
        """Cash the fills would consume, in order: effective entry cost plus any repayment the
        closing proceeds don't cover (the same quantity the mock reserves). Full notional when no
        relationship graph is loaded."""
        held = dict(positions)
        outstanding = dict(self.outstanding)
        total = 0
        for ex, yes_dir, price, qty in legs:
            if qty <= 0:
                continue
            econ = fill_economics(self.graph, held, outstanding, ex, yes_dir * qty, price)
            total += econ.effective_cost_pips + max(0, econ.repayment_pips - econ.close_proceeds_pips)
            held[ex] = held.get(ex, 0) + yes_dir * qty
            if econ.component is not None:
                outstanding[econ.component.id] = econ.outstanding_after_pips
        return total

    def snapshot(self) -> dict[str, object]:
        return {
            "responses": self.responses,
            "withAll": self.with_all,
            "linkedWithoutAll": self.linked_without_all,
            "savings": self.savings_pips / PIPS_PER_UNIT,
            "outstanding": {k: v / PIPS_PER_UNIT for k, v in sorted(self.outstanding.items())},
            "relationships": len(self.relations),
        }
