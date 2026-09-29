"""Ledger invariants: netting pays 1 per cancelled pair, so every fill's payoff is path independent."""

from __future__ import annotations

from datetime import UTC, datetime

from hypothesis import given
from hypothesis import strategies as st
from susq.platform.ledger import Ledger, opening_cost_pips

T = datetime(2026, 10, 1, tzinfo=UTC)
fills = st.lists(
    st.tuples(
        st.integers(min_value=-50, max_value=50).filter(bool),
        st.integers(min_value=1, max_value=199).map(lambda t: t * 50),
    ),
    min_size=1,
    max_size=40,
)


@given(fills)
def test_wealth_in_each_outcome_is_the_sum_of_fill_payoffs(seq: list[tuple[int, int]]) -> None:
    ledger = Ledger(0)
    wealth_yes = wealth_no = 0
    for qty, price in seq:
        ledger.execute("x", qty, price, T)
        # A YES-buy of n at x: +n(1-x) if YES resolves, -n*x if NO. A YES-sell is the mirror image.
        wealth_yes += qty * (10_000 - price)
        wealth_no += -qty * price
    pos = ledger.position("x")
    assert ledger.cash_pips + max(pos, 0) * 10_000 == wealth_yes
    assert ledger.cash_pips + max(-pos, 0) * 10_000 == wealth_no


@given(fills)
def test_position_is_the_signed_sum_and_lots_share_one_side(seq: list[tuple[int, int]]) -> None:
    ledger = Ledger(0)
    for qty, price in seq:
        ledger.execute("x", qty, price, T)
    holding = ledger.holdings["x"]
    assert holding.quantity == sum(q for q, _ in seq)
    assert len({lot.side for lot in holding.lots}) <= 1


def test_buying_the_opposite_side_nets_and_pays_one_per_pair() -> None:
    ledger = Ledger(1_000 * 10_000)
    ledger.execute("x", 10, 6_000, T)  # buy 10 YES @ 0.60
    ex = ledger.execute("x", -10, 6_500, T)  # buy 10 NO @ 0.35 (YES-sell @ 0.65)
    assert ledger.position("x") == 0
    assert ex.closed == 10
    assert ex.realized_pips == 10 * 500
    assert ledger.cash_pips == (1_000 - 6 - 3.5 + 10) * 10_000


def test_fifo_close_and_realized_pnl() -> None:
    ledger = Ledger(0)
    ledger.execute("x", 5, 4_000, T)
    ledger.execute("x", 5, 5_000, T)
    ex = ledger.execute("x", -7, 6_000, T)  # closes the 0.40 lot fully and 2 of the 0.50 lot
    assert ex.realized_pips == 5 * 2_000 + 2 * 1_000
    assert [lot.quantity for lot in ledger.holdings["x"].lots] == [3]
    assert ledger.holdings["x"].realized_pips == ex.realized_pips


def test_opening_cost_and_closable() -> None:
    ledger = Ledger(0)
    ledger.execute("x", -4, 7_000, T)  # 4 NO @ 0.30
    assert ledger.closable("x", +1) == 4
    assert ledger.closable("x", -1) == 0
    assert ledger.opening_cost_pips("x", +1, 7_000, 6) == 2 * 7_000
    assert opening_cost_pips(-1, 7_000, 1) == 3_000


def test_account_value_uses_marks_and_falls_back_to_cost() -> None:
    ledger = Ledger(100 * 10_000)
    ledger.execute("a", 10, 5_000, T)
    ledger.execute("b", -10, 5_000, T)
    assert ledger.account_value_pips({"a": 6_000}) == (100 - 10) * 10_000 + 10 * 6_000 + 10 * 5_000
