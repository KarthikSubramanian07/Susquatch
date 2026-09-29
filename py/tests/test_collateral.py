"""The ALL collateral model: floors, advances, repayments and releases (pure, no HTTP)."""

from __future__ import annotations

import itertools
from datetime import UTC, datetime

import pytest
from hypothesis import given, settings
from hypothesis import strategies as st
from susq.platform.collateral import (
    CollateralAccount,
    CollateralGraph,
    Relation,
    fill_economics,
    payout_pips,
)
from susq.platform.ledger import Ledger
from susq.platform.models import Relationship

T0 = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)
U = 10_000  # pip-units per SUSQie

ME = Relation("me", "mutually_exclusive", ("1", "2"), is_exhaustive=True)
ME_OPEN = Relation("me-open", "mutually_exclusive", ("1", "2"), is_exhaustive=False)
COMP = Relation("comp", "complementary", ("1", "2"))
IMP = Relation("imp", "implication", (), antecedent="3", consequent="1")


def floor(relations: list[Relation], positions: dict[str, int], ex: str = "1") -> int | None:
    graph = CollateralGraph(relations)
    comp = graph.component_of(ex)
    assert comp is not None
    return graph.floor_pips(comp, positions)


# ── floors ──────────────────────────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("relations", "positions", "expected"),
    [
        ([ME], {"1": -10, "2": -10}, 10),  # NO both: exactly one wins, the other NO pays
        ([ME], {"1": 10, "2": 10}, 10),  # YES both on an exhaustive pair
        ([ME], {"1": 10}, 0),  # a lone YES can lose
        ([ME_OPEN], {"1": 10, "2": 10}, 0),  # "neither" is possible (an independent wins)
        ([ME_OPEN], {"1": -10, "2": -10}, 10),  # at most one YES: NO both still pays 1
        ([COMP], {"1": 7, "2": 7}, 7),
        ([IMP], {"3": -5, "1": 5}, 5),  # NO antecedent + YES consequent
        ([IMP], {"3": 5, "1": -5}, 0),  # the wrong way round guarantees nothing
    ],
)
def test_floor_by_relationship_type(
    relations: list[Relation], positions: dict[str, int], expected: int
) -> None:
    assert floor(relations, positions) == expected * U


def test_components_are_unions_of_relationships_sharing_exchanges() -> None:
    graph = CollateralGraph([ME, IMP, Relation("x", "mutually_exclusive", ("8", "9"))])
    comp = graph.component_of("3")
    assert comp is not None
    assert comp.exchange_ids == ("1", "2", "3")
    assert comp.id == "1-2-3"
    assert comp.relationship_ids == ("imp", "me")
    assert graph.component_of("8") is not graph.component_of("1")
    assert graph.component_of("42") is None


def test_numeric_component_ids_sort_numerically() -> None:
    graph = CollateralGraph([Relation("r", "complementary", ("160", "150"))])
    comp = graph.component_of("150")
    assert comp is not None
    assert comp.id == "150-160"  # the documented example


def test_unsupported_types_add_no_constraint_and_say_so() -> None:
    weird = Relation("b", "boolean_and", ("1", "2"))
    comp = CollateralGraph([weird]).component_of("1")
    assert comp is not None
    assert comp.diagnostic is not None
    assert "boolean_and" in comp.diagnostic
    # With no constraint every outcome is allowed: YES + YES can pay nothing.
    assert floor([weird], {"1": 3, "2": 3}) == 0


def test_components_too_large_have_no_floor_and_never_advance() -> None:
    members = tuple(str(i) for i in range(1, 20))
    rel = Relation("big", "mutually_exclusive", members, is_exhaustive=True)
    graph = CollateralGraph([rel], max_exchanges=8)
    comp = graph.component_of("1")
    assert comp is not None
    assert graph.floor_pips(comp, {"1": -1}) is None
    econ = fill_economics(graph, {"1": -1}, {}, "2", -1, 5_000)
    assert econ.savings_pips == 0
    assert econ.component is comp


def test_relation_from_platform_model() -> None:
    rel = Relationship.model_validate(
        {
            "id": "r1",
            "type": "implication",
            "status": "active",
            "isExhaustive": False,
            "nodes": [
                {"exchangeId": "36", "role": "antecedent"},
                {"exchangeId": "37", "role": "consequent"},
            ],
        }
    )
    got = Relation.from_model(rel)
    assert (got.antecedent, got.consequent, got.constrains) == ("36", "37", True)
    via_direction = Relationship.model_validate(
        {
            "id": "r2",
            "type": "monotonic",
            "nodes": [{"exchangeId": "5", "role": "member"}, {"exchangeId": "6", "role": "member"}],
            "direction": {
                "kind": "antecedent_to_consequent",
                "fromExchangeIds": ["6"],
                "toExchangeIds": ["5"],
                "description": "",
            },
        }
    )
    got = Relation.from_model(via_direction)
    assert (got.antecedent, got.consequent) == ("6", "5")
    stale = Relationship.model_validate({"id": "r3", "type": "complementary", "status": "stale"})
    assert Relation.from_models([rel, stale]) == [Relation.from_model(rel)]


# ── advances, repayments, releases ──────────────────────────────────────────────────────────


def test_second_leg_of_a_no_bundle_is_backed_by_the_floor() -> None:
    graph = CollateralGraph([ME])
    acct = CollateralAccount(graph)
    ledger = Ledger(100 * U)
    first = acct.execute(ledger, "1", -10, 4_000, T0)  # buy NO on 1 @ 0.60
    assert (first.full_cost_pips, first.savings_pips) == (6 * U, 0)
    second = acct.execute(ledger, "2", -10, 5_500, T0)  # buy NO on 2 @ 0.45
    assert second.full_cost_pips == 45_000
    assert second.savings_pips == 45_000  # floor 10 covers the whole 4.5
    assert second.effective_cost_pips == 0
    assert second.floor_after_pips == 10 * U
    assert acct.outstanding == {"1-2": 45_000}
    assert ledger.cash_pips == 100 * U - 6 * U
    assert [e.type for e in acct.events] == ["ALL_COLLATERAL_ADVANCE"]


def test_advance_is_capped_by_floor_minus_outstanding() -> None:
    graph = CollateralGraph([ME])
    acct = CollateralAccount(graph)
    ledger = Ledger(100 * U)
    acct.execute(ledger, "1", 10, 9_000, T0)  # YES on 1 @ 0.90: 9.0
    econ = acct.execute(ledger, "2", 10, 9_500, T0)  # YES on 2 @ 0.95: 9.5, floor 10
    assert econ.savings_pips == 95_000
    econ = acct.execute(ledger, "2", 1, 9_500, T0)  # floor is still 10 (11 YES on 2 can lose)
    assert econ.savings_pips == 5_000  # only 10 - 9.5 left to advance
    assert acct.outstanding["1-2"] == 10 * U


def test_selling_backing_shares_repays_and_closing_all_clears_the_advance() -> None:
    graph = CollateralGraph([ME])
    acct = CollateralAccount(graph)
    ledger = Ledger(100 * U)
    acct.execute(ledger, "1", -10, 4_000, T0)
    acct.execute(ledger, "2", -10, 5_500, T0)
    econ = acct.execute(ledger, "2", 4, 5_000, T0)  # close 4 NO on 2 (buy YES nets FIFO) @ 0.50
    assert econ.close_proceeds_pips == 4 * 5_000
    assert econ.floor_after_pips == 6 * U
    assert econ.repayment_pips == 0  # outstanding 4.5 <= floor 6
    econ = acct.execute(ledger, "2", 6, 5_000, T0)  # close the rest: floor drops to 0
    assert econ.repayment_pips == 45_000
    assert acct.outstanding == {}
    assert acct.events[-1].type == "ALL_COLLATERAL_REPAYMENT"
    assert ledger.cash_pips >= 0


def test_settlement_releases_the_advance_out_of_the_payout() -> None:
    graph = CollateralGraph([ME])
    acct = CollateralAccount(graph)
    ledger = Ledger(10 * U)
    acct.execute(ledger, "1", -10, 4_000, T0)
    acct.execute(ledger, "2", -10, 5_500, T0)
    cash_before = ledger.cash_pips
    graph.settle("1", 1)  # 1 wins: NO on 2 pays 10
    ledger.settle("1", yes_won=True)
    released = acct.after_settlement(ledger, "1", T0)
    assert released == 0  # the remaining NO on 2 is now certain: floor 10 >= 4.5
    graph.settle("2", 0)
    _, _, payout = ledger.settle("2", yes_won=False)
    released = acct.after_settlement(ledger, "2", T0)
    assert payout == 10 * U
    assert released == 45_000
    assert acct.outstanding == {}
    assert ledger.cash_pips == cash_before + payout - released
    assert acct.events[-1].type == "ALL_COLLATERAL_RELEASE"


def test_settling_against_a_relationship_is_refused() -> None:
    graph = CollateralGraph([ME])
    graph.settle("1", 1)
    assert not graph.consistent("2", 1)
    with pytest.raises(ValueError, match="contradicts"):
        graph.settle("2", 1)


def test_affordable_stops_a_repayment_from_overdrawing_cash() -> None:
    graph = CollateralGraph([ME])
    acct = CollateralAccount(graph)
    ledger = Ledger(6 * U)
    acct.execute(ledger, "1", -10, 4_000, T0)  # cash 0 left
    acct.execute(ledger, "2", -10, 5_500, T0)  # fully advanced
    assert ledger.cash_pips == 0
    # Closing NO on 1 at 0.99 YES (proceeds 0.01/share) must repay the advance: unaffordable.
    assert acct.affordable(ledger, "1", 1, 9_900, 10) < 10
    n = acct.affordable(ledger, "1", 1, 9_900, 10)
    acct.execute(ledger, "1", n, 9_900, T0)
    assert ledger.cash_pips >= 0


def test_required_cash_nets_the_prospective_advance() -> None:
    graph = CollateralGraph([ME])
    acct = CollateralAccount(graph)
    ledger = Ledger(100 * U)
    acct.execute(ledger, "1", -10, 4_000, T0)
    need, credit = acct.required_cash_pips(ledger, "2", -1, 5_500, 10, 10)
    assert (need, credit) == (0, 45_000)
    off = CollateralAccount(graph, enabled=False)
    assert off.required_cash_pips(ledger, "2", -1, 5_500, 10, 10) == (45_000, 0)


def test_disabled_account_is_plain_cash_accounting() -> None:
    acct = CollateralAccount(CollateralGraph([ME]), enabled=False)
    ledger = Ledger(100 * U)
    acct.execute(ledger, "1", -10, 4_000, T0)
    econ = acct.execute(ledger, "2", -10, 5_500, T0)
    assert econ.component is None
    assert econ.savings_pips == 0
    assert ledger.cash_pips == 100 * U - 6 * U - 45_000
    assert acct.views(ledger) == []


# ── properties ──────────────────────────────────────────────────────────────────────────────

EXCHANGES = ("1", "2", "3", "4")


@st.composite
def relation_sets(draw: st.DrawFn) -> list[Relation]:
    rels: list[Relation] = []
    for i in range(draw(st.integers(1, 3))):
        kind = draw(st.sampled_from(["mutually_exclusive", "complementary", "implication", "boolean_or"]))
        if kind == "implication":
            a, c = draw(st.permutations(EXCHANGES))[:2]
            rels.append(Relation(f"r{i}", kind, (), antecedent=a, consequent=c))
        else:
            size = 2 if kind == "complementary" else draw(st.integers(2, 4))
            members = tuple(draw(st.permutations(EXCHANGES))[:size])
            rels.append(Relation(f"r{i}", kind, members, is_exhaustive=draw(st.booleans())))
    return rels


def brute_force_outcomes(rels: list[Relation], exchanges: tuple[str, ...]) -> list[tuple[int, ...]]:
    """Independent of the graph's pruned enumeration: every assignment, filtered."""
    out = []
    for bits in itertools.product((0, 1), repeat=len(exchanges)):
        outcome = dict(zip(exchanges, bits, strict=True))
        if all(r.allows(outcome) for r in rels):
            out.append(bits)
    return out


@settings(max_examples=300, deadline=None)
@given(rels=relation_sets(), qty=st.lists(st.integers(-20, 20), min_size=4, max_size=4))
def test_floor_is_the_exact_minimum_payout_over_allowed_outcomes(
    rels: list[Relation], qty: list[int]
) -> None:
    graph = CollateralGraph(rels)
    positions = dict(zip(EXCHANGES, qty, strict=True))
    for comp in graph.components.values():
        members = [r for r in rels if r in comp.relations]
        allowed = brute_force_outcomes(members, comp.exchange_ids)
        got = graph.floor_pips(comp, positions)
        if not allowed:
            assert got is None
            continue
        payouts = [payout_pips(positions, comp.exchange_ids, o) for o in allowed]
        assert got is not None
        assert all(got <= p for p in payouts)  # a true lower bound...
        assert got == min(payouts)  # ...and tight


@settings(max_examples=300, deadline=None)
@given(
    rels=relation_sets(),
    fills=st.lists(
        st.tuples(st.sampled_from(EXCHANGES), st.integers(-15, 15), st.integers(1, 199)),
        max_size=25,
    ),
)
def test_advance_never_exceeds_floor_and_cash_never_goes_negative(
    rels: list[Relation], fills: list[tuple[str, int, int]]
) -> None:
    graph = CollateralGraph(rels)
    acct = CollateralAccount(graph)
    ledger = Ledger(20 * U)
    for ex, yes_qty, tick in fills:
        if yes_qty == 0:
            continue
        price = tick * 50
        n = acct.affordable(ledger, ex, 1 if yes_qty > 0 else -1, price, abs(yes_qty))
        if n:
            acct.execute(ledger, ex, n if yes_qty > 0 else -n, price, T0)
        assert ledger.cash_pips >= 0
        for comp in graph.components.values():
            floor_now = acct.floor_pips(comp, ledger) or 0
            assert 0 <= acct.outstanding.get(comp.id, 0) <= floor_now
