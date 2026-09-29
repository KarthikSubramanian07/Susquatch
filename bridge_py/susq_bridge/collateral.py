"""ALL collateral in the bridge: record what the engine reports, estimate what it would charge.

Two jobs, deliberately separate:

* **observe** (always on): every order response's ``all`` block is folded into per-component
  outstanding advances and kept for the day-one comparison; ``GET /portfolio/collateral`` reads
  replace the outstanding map wholesale. The gateway journals each one as ``all_economics``.
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

from susq.platform.collateral import CollateralGraph, Relation, fill_economics
from susq.platform.models import Collateral, OrderRequest, PlaceOrderResult, Relationship
from susq.platform.prices import PIPS_PER_UNIT

__all__ = ["CollateralTracker", "FillLeg", "ObservedEconomics", "request_legs"]

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
        }


class CollateralTracker:
    def __init__(self, *, keep: int = 500) -> None:
        self.graph: CollateralGraph | None = None
        self.relations: list[Relation] = []
        #: component id -> outstanding advance (pip-units), as last reported by the engine
        self.outstanding: dict[str, int] = {}
        self.observed: deque[ObservedEconomics] = deque(maxlen=keep)
        self.responses = 0
        self.with_all = 0
        self.savings_pips = 0

    # ── relationships ───────────────────────────────────────────────────────────────────────

    def set_relationships(self, relationships: Iterable[Relationship]) -> list[Relation]:
        """Rebuild the model graph from ``GET /relationships`` (active ones only)."""
        self.relations = Relation.from_models(relationships)
        self.graph = CollateralGraph(self.relations)
        return self.relations

    # ── observe ─────────────────────────────────────────────────────────────────────────────

    def observe(
        self, intent_id: str, request: OrderRequest, result: PlaceOrderResult
    ) -> ObservedEconomics | None:
        """Fold one order response in. Returns the economics if the engine produced a quote."""
        self.responses += 1
        econ = result.all
        if econ is None:
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
            "savings": self.savings_pips / PIPS_PER_UNIT,
            "outstanding": {k: v / PIPS_PER_UNIT for k, v in sorted(self.outstanding.items())},
            "relationships": len(self.relations),
        }
