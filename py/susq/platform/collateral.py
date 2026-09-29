"""ALL collateral: guaranteed payout floors and relationship-backed advances.

Shared by the mock exchange, the paper broker and the bridge's buying-power estimate, so all three
compute the same numbers.

**Documented** (official reference, ``AllExecutionEconomics`` / ``PortfolioCollateral``):

* Active ALL relationships link exchanges into *connected components*; the engine evaluates each
  component a portfolio touches.
* A component's **guaranteed payout floor** is the minimum, over every joint outcome the
  relationships allow, of the component's total payout ("null when unavailable").
* Part of a new fill's nominal cost can be backed by that floor instead of cash (a **collateral
  advance**); the order response reports ``fullNotionalCost``, ``effectiveEntryCost`` (cash actually
  debited), ``collateralSavings``, ``collateralRepayment`` ("advance repaid because this execution
  reduced the component's guaranteed backing"), ``outstandingAdvanceAfter``,
  ``guaranteedPayoutFloorAfter`` and ``netBuyingPowerImpact`` (positive consumes buying power).
* Transactions carry ``ALL_COLLATERAL_ADVANCE``, ``ALL_COLLATERAL_REPAYMENT`` and
  ``ALL_COLLATERAL_RELEASE``.

**MODEL ASSUMPTIONS** (not documented; every one is switchable by disabling collateral):

* A1. Fills are evaluated one at a time, in order. After each fill the advance may grow to
  ``min(fill's opening cost, floor_after - outstanding)``; it never backs cost already paid in cash.
* A2. If a fill leaves ``outstanding > floor_after`` (it closed backing shares), the difference is
  repaid from cash at once, and no advance is granted on that fill.
* A3. On settlement, the outstanding advance above the remaining component's floor (conditioned on
  the settled outcomes) is repaid out of the payout (``ALL_COLLATERAL_RELEASE``); once every
  exchange of the component has settled, nothing is outstanding. The payout always covers it.
* A4. The outstanding advance is a liability: account value = cash + holdings - outstanding.
* A5. ``mutually_exclusive``: at most one member resolves YES (exactly one when ``isExhaustive``);
  ``complementary``: exactly one of its two members; ``implication``/``monotonic`` with one
  antecedent and one consequent: antecedent YES implies consequent YES. Other types (boolean
  expressions, multi-node monotonic chains) add **no** constraint: ignoring a constraint only lowers
  the floor, so the model stays a valid lower bound.
* A6. Components larger than :data:`MAX_COMPONENT_EXCHANGES` exchanges are not enumerated: their
  floor is unavailable (``None``) and they never back an advance.
* A7. The component id is the sorted covered exchange ids joined with ``-`` (the documented example
  is ``"150-160"`` for exchanges 150 and 160). It is opaque; never parse it.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime
from typing import TYPE_CHECKING, Any

from susq.platform.ledger import Ledger, opening_cost_pips, units
from susq.platform.prices import PIPS_PER_UNIT

if TYPE_CHECKING:
    from susq.platform.models import Relationship

__all__ = [
    "MAX_COMPONENT_EXCHANGES",
    "AllTotals",
    "CollateralAccount",
    "CollateralEvent",
    "CollateralGraph",
    "Component",
    "FillEconomics",
    "Relation",
    "fill_economics",
    "payout_pips",
]

#: Largest component the model enumerates (2^16 assignments, once per component, cached).
MAX_COMPONENT_EXCHANGES = 16

ADVANCE = "ALL_COLLATERAL_ADVANCE"
REPAYMENT = "ALL_COLLATERAL_REPAYMENT"
RELEASE = "ALL_COLLATERAL_RELEASE"

Outcome = tuple[int, ...]  # one 0/1 per exchange of a component, in component order


def _numeric_key(exchange_id: str) -> tuple[int, str]:
    return (int(exchange_id), exchange_id) if exchange_id.isdigit() else (1 << 62, exchange_id)


@dataclass(frozen=True, slots=True)
class Relation:
    """One active relationship, reduced to what the floor needs."""

    id: str
    type: str
    members: tuple[str, ...]
    antecedent: str | None = None
    consequent: str | None = None
    is_exhaustive: bool = False

    @property
    def exchange_ids(self) -> tuple[str, ...]:
        extra = tuple(e for e in (self.antecedent, self.consequent) if e is not None)
        return tuple(dict.fromkeys((*self.members, *extra)))

    @property
    def constrains(self) -> bool:
        """False for types the model does not understand (they add no constraint, see A5)."""
        if self.type == "mutually_exclusive":
            return len(self.members) >= 2
        if self.type == "complementary":
            return len(self.members) == 2
        if self.type in ("implication", "monotonic"):
            return self.antecedent is not None and self.consequent is not None
        return False

    def allows(self, outcome: Mapping[str, int]) -> bool:
        if not self.constrains:
            return True
        if self.type == "mutually_exclusive":
            yes = sum(outcome[m] for m in self.members)
            return yes == 1 if self.is_exhaustive else yes <= 1
        if self.type == "complementary":
            return sum(outcome[m] for m in self.members) == 1
        assert self.antecedent is not None
        assert self.consequent is not None
        return not (outcome[self.antecedent] == 1 and outcome[self.consequent] == 0)

    @classmethod
    def from_model(cls, rel: Relationship) -> Relation:
        """From a ``GET /relationships`` item (roles first, then ``direction`` for implications)."""
        ante = rel.exchange_ids("antecedent")
        cons = rel.exchange_ids("consequent")
        if rel.type in ("implication", "monotonic") and rel.direction is not None:
            ante = ante or list(rel.direction.from_exchange_ids)
            cons = cons or list(rel.direction.to_exchange_ids)
        members = rel.exchange_ids("member") or [e for e in rel.exchange_ids() if e not in (*ante, *cons)]
        single = len(ante) == 1 and len(cons) == 1
        return cls(
            id=rel.id,
            type=rel.type,
            members=tuple(members),
            antecedent=ante[0] if single else None,
            consequent=cons[0] if single else None,
            is_exhaustive=rel.is_exhaustive,
        )

    @classmethod
    def from_models(cls, rels: Iterable[Relationship]) -> list[Relation]:
        """Active relationships only (``status`` absent counts as active)."""
        return [cls.from_model(r) for r in rels if r.status in (None, "active")]


@dataclass(frozen=True, slots=True)
class Component:
    """A connected set of exchanges linked by relationships."""

    id: str
    exchange_ids: tuple[str, ...]
    relationship_ids: tuple[str, ...]
    relations: tuple[Relation, ...]
    diagnostic: str | None = None


def payout_pips(positions: Mapping[str, int], exchange_ids: Sequence[str], outcome: Outcome) -> int:
    """What signed positions (+YES / -NO) pay in one joint outcome."""
    shares = 0
    for ex, yes in zip(exchange_ids, outcome, strict=True):
        qty = positions.get(ex, 0)
        shares += qty * yes if qty > 0 else -qty * (1 - yes)
    return shares * PIPS_PER_UNIT


class CollateralGraph:
    """Components, their allowed joint outcomes, and settled outcomes (global, not per account)."""

    def __init__(
        self, relations: Iterable[Relation], *, max_exchanges: int = MAX_COMPONENT_EXCHANGES
    ) -> None:
        self.relations = [r for r in relations if len(r.exchange_ids) >= 1]
        self.max_exchanges = max_exchanges
        self.settled: dict[str, int] = {}
        self._outcomes: dict[str, list[Outcome] | None] = {}
        parent: dict[str, str] = {}

        def find(x: str) -> str:
            parent.setdefault(x, x)
            while parent[x] != x:
                parent[x] = parent[parent[x]]
                x = parent[x]
            return x

        for rel in self.relations:
            first, *rest = rel.exchange_ids
            find(first)
            for ex in rest:
                parent[find(ex)] = find(first)
        groups: dict[str, list[Relation]] = {}
        for rel in self.relations:
            groups.setdefault(find(rel.exchange_ids[0]), []).append(rel)
        self._by_exchange: dict[str, Component] = {}
        self.components: dict[str, Component] = {}
        for rels in groups.values():
            exchanges = tuple(sorted({e for r in rels for e in r.exchange_ids}, key=_numeric_key))
            ignored = sorted({r.type for r in rels if not r.constrains})
            notes = []
            if len(exchanges) > max_exchanges:
                notes.append(f"component spans {len(exchanges)} exchanges (> {max_exchanges}); not evaluated")
            if ignored:
                notes.append(f"no constraint modeled for: {', '.join(ignored)}")
            comp = Component(
                id="-".join(exchanges),
                exchange_ids=exchanges,
                relationship_ids=tuple(sorted(r.id for r in rels)),
                relations=tuple(rels),
                diagnostic="; ".join(notes) or None,
            )
            self.components[comp.id] = comp
            for ex in exchanges:
                self._by_exchange[ex] = comp

    def __deepcopy__(self, memo: dict[int, Any]) -> CollateralGraph:
        # Shared, never rolled back: relationships are static and settlement is not transactional,
        # so an engine snapshot (atomic multi-leg) keeps pointing every account at the live graph.
        return self

    def component_of(self, exchange_id: str) -> Component | None:
        return self._by_exchange.get(exchange_id)

    # ── outcomes ────────────────────────────────────────────────────────────────────────────

    def outcomes(self, comp: Component) -> list[Outcome] | None:
        """Allowed joint outcomes given what has settled (``None`` when too large to enumerate)."""
        if comp.id not in self._outcomes:
            self._outcomes[comp.id] = self._enumerate(comp, self.settled)
        return self._outcomes[comp.id]

    def _enumerate(self, comp: Component, fixed: Mapping[str, int]) -> list[Outcome] | None:
        exchanges = comp.exchange_ids
        if len(exchanges) > self.max_exchanges:
            return None
        index = {ex: i for i, ex in enumerate(exchanges)}
        # Check each relation as soon as its last exchange is assigned (depth-first with pruning).
        due: dict[int, list[Relation]] = {}
        for rel in comp.relations:
            if rel.constrains:
                due.setdefault(max(index[e] for e in rel.exchange_ids), []).append(rel)
        out: list[Outcome] = []
        assignment: dict[str, int] = {}

        def walk(depth: int) -> None:
            if depth == len(exchanges):
                out.append(tuple(assignment[e] for e in exchanges))
                return
            ex = exchanges[depth]
            for value in (fixed[ex],) if ex in fixed else (0, 1):
                assignment[ex] = value
                if all(rel.allows(assignment) for rel in due.get(depth, ())):
                    walk(depth + 1)
            del assignment[ex]

        walk(0)
        return out

    def floor_pips(self, comp: Component, positions: Mapping[str, int]) -> int | None:
        """Guaranteed payout floor of ``positions`` in ``comp`` (``None`` when unavailable)."""
        outcomes = self.outcomes(comp)
        if not outcomes:
            return None
        return min(payout_pips(positions, comp.exchange_ids, o) for o in outcomes)

    # ── settlement ──────────────────────────────────────────────────────────────────────────

    def consistent(self, exchange_id: str, yes: int) -> bool:
        """Whether settling ``exchange_id`` to ``yes`` leaves at least one allowed outcome."""
        comp = self.component_of(exchange_id)
        if comp is None or len(comp.exchange_ids) > self.max_exchanges:
            return True
        return bool(self._enumerate(comp, {**self.settled, exchange_id: yes}))

    def settle(self, exchange_id: str, yes: int) -> None:
        if not self.consistent(exchange_id, yes):
            raise ValueError(f"settling {exchange_id} to {yes} contradicts its relationships")
        self.settled[exchange_id] = yes
        comp = self.component_of(exchange_id)
        if comp is not None:
            self._outcomes.pop(comp.id, None)


@dataclass(frozen=True, slots=True)
class FillEconomics:
    """What one fill does to cash and to its component's advance (all money in pip-units)."""

    full_cost_pips: int  # nominal cost of the shares the fill opens
    close_proceeds_pips: int  # cash the fill's closing shares release
    savings_pips: int
    repayment_pips: int
    component: Component | None
    floor_after_pips: int | None
    outstanding_after_pips: int

    @property
    def effective_cost_pips(self) -> int:
        return self.full_cost_pips - self.savings_pips

    @property
    def cash_delta_pips(self) -> int:
        """Total change in cash: proceeds - effective cost - repayment."""
        return self.close_proceeds_pips - self.effective_cost_pips - self.repayment_pips


def fill_economics(
    graph: CollateralGraph | None,
    positions: Mapping[str, int],
    outstanding: Mapping[str, int],
    exchange_id: str,
    yes_qty: int,
    yes_price_pips: int,
) -> FillEconomics:
    """Economics of one fill of ``yes_qty`` YES shares (+buy YES / -buy NO) against ``positions``.

    Pure: the caller applies it. ``graph=None`` (collateral off) or an unlinked exchange yields plain
    cash accounting with no advance.
    """
    pos = positions.get(exchange_id, 0)
    closed = min(abs(yes_qty), abs(pos)) if pos * yes_qty < 0 else 0
    opened = abs(yes_qty) - closed
    yes_dir = 1 if yes_qty > 0 else -1
    full = opening_cost_pips(yes_dir, yes_price_pips, opened)
    close_unit = yes_price_pips if yes_dir < 0 else PIPS_PER_UNIT - yes_price_pips
    proceeds = closed * close_unit
    comp = graph.component_of(exchange_id) if graph is not None else None
    if comp is None or graph is None:
        return FillEconomics(full, proceeds, 0, 0, None, None, 0)
    after = {ex: positions.get(ex, 0) for ex in comp.exchange_ids}
    after[exchange_id] = pos + yes_qty
    floor = graph.floor_pips(comp, after)
    held = outstanding.get(comp.id, 0)
    backing = 0 if floor is None else floor
    if held > backing:
        return FillEconomics(full, proceeds, 0, held - backing, comp, floor, backing)
    savings = min(full, backing - held)
    return FillEconomics(full, proceeds, savings, 0, comp, floor, held + savings)


@dataclass(frozen=True, slots=True)
class CollateralEvent:
    """One ALL collateral transaction (advance, repayment or release)."""

    type: str
    component_id: str
    delta_pips: int  # + advance granted, - advance repaid/released
    outstanding_after_pips: int
    reason: str
    exchange_id: str
    at: datetime


@dataclass
class AllTotals:
    """Per-order accumulation of fill economics, rendered as the wire ``all`` object."""

    component: Component | None = None
    full_pips: int = 0
    savings_pips: int = 0
    repayment_pips: int = 0
    cash_delta_pips: int = 0

    def add(self, econ: FillEconomics) -> None:
        self.component = self.component or econ.component
        self.full_pips += econ.full_cost_pips
        self.savings_pips += econ.savings_pips
        self.repayment_pips += econ.repayment_pips
        self.cash_delta_pips += econ.cash_delta_pips

    def wire(self, account: CollateralAccount, ledger: Ledger, exchange_id: str) -> dict[str, Any] | None:
        """``AllExecutionEconomics`` for this order, or ``None`` without a relationship quote."""
        comp = self.component or account.component(exchange_id)
        if comp is None:
            return None
        floor = account.floor_pips(comp, ledger)
        return {
            "fullNotionalCost": units(self.full_pips),
            "effectiveEntryCost": units(self.full_pips - self.savings_pips),
            "collateralSavings": units(self.savings_pips),
            "relationshipIds": list(comp.relationship_ids),
            "componentId": comp.id,
            "guaranteedPayoutFloorAfter": units(floor or 0),
            "outstandingAdvanceAfter": units(account.outstanding.get(comp.id, 0)),
            "collateralRepayment": units(self.repayment_pips),
            "netBuyingPowerImpact": units(-self.cash_delta_pips),
        }


@dataclass
class CollateralAccount:
    """One account's advances. ``enabled=False`` (or no graph) restores plain cash accounting."""

    graph: CollateralGraph | None
    enabled: bool = True
    outstanding: dict[str, int] = field(default_factory=dict)
    events: list[CollateralEvent] = field(default_factory=list)

    def component(self, exchange_id: str) -> Component | None:
        if not self.enabled or self.graph is None:
            return None
        return self.graph.component_of(exchange_id)

    @property
    def liability_pips(self) -> int:
        return sum(self.outstanding.values())

    def floor_pips(self, comp: Component, ledger: Ledger) -> int | None:
        assert self.graph is not None
        return self.graph.floor_pips(comp, {ex: ledger.position(ex) for ex in comp.exchange_ids})

    # ── fills ───────────────────────────────────────────────────────────────────────────────

    def preview(self, ledger: Ledger, exchange_id: str, yes_qty: int, yes_price_pips: int) -> FillEconomics:
        graph = self.graph if self.enabled else None
        comp = graph.component_of(exchange_id) if graph is not None else None
        exchanges = comp.exchange_ids if comp is not None else (exchange_id,)
        positions = {ex: ledger.position(ex) for ex in exchanges}
        return fill_economics(graph, positions, self.outstanding, exchange_id, yes_qty, yes_price_pips)

    def execute(
        self, ledger: Ledger, exchange_id: str, yes_qty: int, yes_price_pips: int, at: datetime
    ) -> FillEconomics:
        """Apply a fill to ``ledger`` (FIFO) plus its advance or repayment."""
        econ = self.preview(ledger, exchange_id, yes_qty, yes_price_pips)
        ledger.execute(exchange_id, yes_qty, yes_price_pips, at)
        comp = econ.component
        if comp is None:
            return econ
        ledger.cash_pips += econ.savings_pips - econ.repayment_pips
        self._set(comp.id, econ.outstanding_after_pips)
        if econ.savings_pips:
            self._event(ADVANCE, comp, econ.savings_pips, "relationship-backed entry", exchange_id, at)
        if econ.repayment_pips:
            self._event(REPAYMENT, comp, -econ.repayment_pips, "guaranteed backing reduced", exchange_id, at)
        return econ

    def affordable(self, ledger: Ledger, exchange_id: str, yes_dir: int, yes_price_pips: int, n: int) -> int:
        """Largest ``m <= n`` whose fill leaves cash >= 0.

        Reservations already keep plain limit fills solvent; this is the backstop for what they can't
        foresee (a repayment, or an advance smaller than the one credited at placement).
        """
        if n <= 0:
            return 0

        def ok(m: int) -> bool:
            econ = self.preview(ledger, exchange_id, yes_dir * m, yes_price_pips)
            return ledger.cash_pips + econ.cash_delta_pips >= 0

        if ok(n):
            return n
        lo, hi = 0, n  # ok(lo) holds, ok(hi) fails
        while hi - lo > 1:
            mid = (lo + hi) // 2
            lo, hi = (mid, hi) if ok(mid) else (lo, mid)
        return lo

    def required_cash_pips(
        self,
        ledger: Ledger,
        exchange_id: str,
        yes_dir: int,
        yes_limit_pips: int,
        quantity: int,
        reserve_qty: int,
    ) -> tuple[int, int]:
        """(cash the order needs available, advance credited against its reservation).

        Without a linked component this is the plain opening cost of ``reserve_qty``. With one, the
        prospective advance (fills at the limit, in full) is credited, and a closing order must also
        cover any repayment its proceeds would not.
        """
        base = opening_cost_pips(yes_dir, yes_limit_pips, reserve_qty)
        if self.component(exchange_id) is None:
            return base, 0
        econ = self.preview(ledger, exchange_id, yes_dir * quantity, yes_limit_pips)
        credit = min(base, econ.savings_pips)
        shortfall = max(0, econ.repayment_pips - econ.close_proceeds_pips)
        return base - credit + shortfall, credit

    # ── settlement ──────────────────────────────────────────────────────────────────────────

    def after_settlement(self, ledger: Ledger, exchange_id: str, at: datetime) -> int:
        """Release what the remaining component can no longer back. Returns pip-units repaid."""
        comp = self.component(exchange_id)
        if comp is None:
            return 0
        held = self.outstanding.get(comp.id, 0)
        floor = self.floor_pips(comp, ledger) or 0
        release = max(0, held - floor)
        if release:
            ledger.cash_pips -= release
            self._set(comp.id, held - release)
            self._event(RELEASE, comp, -release, "settlement", exchange_id, at)
        return release

    # ── reads ───────────────────────────────────────────────────────────────────────────────

    def views(self, ledger: Ledger) -> list[dict[str, Any]]:
        """``PortfolioCollateral`` rows (without profile/tournament) for every touched component."""
        if not self.enabled or self.graph is None:
            return []
        rows = []
        for comp in self.graph.components.values():
            touched = any(ledger.position(ex) for ex in comp.exchange_ids)
            held = self.outstanding.get(comp.id, 0)
            if not touched and not held:
                continue
            floor = self.floor_pips(comp, ledger)
            rows.append(
                {
                    "outstandingAdvance": units(held),
                    "componentId": comp.id,
                    "coveredExchangeIds": [int(e) if e.isdigit() else e for e in comp.exchange_ids],
                    "relationshipIds": list(comp.relationship_ids),
                    "guaranteedPayoutFloor": None if floor is None else units(floor),
                    "diagnostic": comp.diagnostic,
                }
            )
        return rows

    def _set(self, component_id: str, value: int) -> None:
        if value:
            self.outstanding[component_id] = value
        else:
            self.outstanding.pop(component_id, None)

    def _event(self, kind: str, comp: Component, delta: int, reason: str, ex: str, at: datetime) -> None:
        after = self.outstanding.get(comp.id, 0)
        self.events.append(CollateralEvent(kind, comp.id, delta, after, reason, ex, at))
