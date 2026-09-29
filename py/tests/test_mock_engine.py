"""Matching-engine semantics, exercised directly (no HTTP)."""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import Any

import pytest
from susq.mock.engine import Account, Engine, EngineError, MarketDef, iso
from susq.platform.ledger import Ledger

T0 = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)
EX = "11"


@pytest.fixture
def eng() -> Engine:
    e = Engine("t", [MarketDef("1", EX, "Will it?")])
    for name in ("a", "b", "c"):
        e.add_account(Account(name, name, Ledger(1_000 * 10_000)))
    return e


def body(side: str, action: str, qty: Any, price: Any = None, **extra: Any) -> dict[str, Any]:
    out: dict[str, Any] = {
        "exchangeId": EX,
        "side": side,
        "action": action,
        "quantity": qty,
        "tournamentId": "t",
    }
    if price is not None:
        out["price"] = price
    out.update(extra)
    return out


def place(eng: Engine, who: str, *args: Any, at: datetime = T0, **kw: Any) -> dict[str, Any]:
    return eng.place(who, body(*args, **kw), at).body


def test_price_time_priority_and_depth_walk(eng: Engine) -> None:
    place(eng, "b", "no", "buy", 5, 0.40)  # ask YES 0.60, first in time
    place(eng, "c", "no", "buy", 5, 0.40)  # ask YES 0.60, second
    place(eng, "c", "no", "buy", 5, 0.45)  # ask YES 0.55, better price
    res = place(eng, "a", "yes", "buy", 12, 0.60)
    assert res["quantityTraded"] == 12
    assert res["open"] is False
    assert res["totalCost"] == pytest.approx(5 * 0.55 + 7 * 0.60)
    b_orders = [o for o in eng.state.orders.values() if o.owner == "b"]
    c_060 = [o for o in eng.state.orders.values() if o.owner == "c" and o.yes_price == 6_000]
    assert b_orders[0].status == "filled"
    assert c_060[0].remaining == 3


@pytest.mark.parametrize(
    "bad",
    [
        {"price": 0.123},
        {"price": 0.999},
        {"price": 0.0, "action": "buy"},
        {"price": 1.2},
        {"quantity": 1.5},
        {"quantity": 0},
        {"quantity": True},
        {"side": "maybe"},
        {"exchangeId": 11},
    ],
)
def test_validation_errors_are_400(eng: Engine, bad: dict[str, Any]) -> None:
    b = body("yes", "buy", 1, 0.5)
    b.update(bad)
    with pytest.raises(EngineError) as info:
        eng.place("a", b, T0)
    assert (info.value.status, info.value.code) == (400, "VALIDATION_ERROR")


def test_unknown_exchange_and_wrong_tournament_are_404(eng: Engine) -> None:
    with pytest.raises(EngineError) as info:
        eng.place("a", {**body("yes", "buy", 1, 0.5), "exchangeId": "99"}, T0)
    assert info.value.status == 404
    with pytest.raises(EngineError) as info:
        eng.place("a", {**body("yes", "buy", 1, 0.5), "tournamentId": "other"}, T0)
    assert info.value.status == 404


def test_market_order_fills_then_cancels_remainder(eng: Engine) -> None:
    place(eng, "b", "no", "buy", 3, 0.40)
    res = place(eng, "a", "yes", "buy", 10)  # price omitted -> market
    assert res["quantityTraded"] == 3
    assert res["open"] is False
    assert res["remainingQuantity"] == 0
    res = place(eng, "a", "yes", "buy", 1, 1)  # price 1 on a buy is also a market order
    assert res["quantityTraded"] == 0


def test_market_orders_reject_expiration(eng: Engine) -> None:
    with pytest.raises(EngineError):
        place(eng, "a", "yes", "buy", 1, expirationDate=iso(T0 + timedelta(minutes=1)))


def test_flat_sell_is_canonicalized_to_complement_buy(eng: Engine) -> None:
    res = place(eng, "a", "yes", "sell", 10, 0.60)
    assert (res["action"], res["side"], res["price"], res["quantity"]) == ("buy", "no", 0.4, 10)
    assert res["open"] is True


def test_backed_sell_closes_conventionally_and_oversized_sell_flips(eng: Engine) -> None:
    place(eng, "b", "no", "buy", 10, 0.50)
    place(eng, "a", "yes", "buy", 10, 0.50)
    assert eng.account("a").ledger.position(EX) == 10
    res = place(eng, "a", "yes", "sell", 6, 0.90)
    assert (res["action"], res["side"]) == ("sell", "yes")
    # Only 4 unencumbered YES remain, so a sell of 5 becomes buy NO.
    res = place(eng, "a", "yes", "sell", 5, 0.95)
    assert (res["action"], res["side"], res["price"]) == ("buy", "no", 0.05)


def test_opposite_buy_nets_and_pays_one(eng: Engine) -> None:
    place(eng, "b", "no", "buy", 10, 0.40)  # b offers YES at 0.60
    place(eng, "a", "yes", "buy", 10, 0.60)
    place(eng, "c", "yes", "buy", 10, 0.65)  # c bids YES 0.65
    res = place(eng, "a", "no", "buy", 10, 0.35)  # a buys NO at 0.35 against c's bid
    assert res["quantityTraded"] == 10
    ledger = eng.account("a").ledger
    assert ledger.position(EX) == 0
    assert ledger.cash == pytest.approx(1_000 - 6.0 - 3.5 + 10)


def test_self_trade_prevention_cancels_crossing_remainder(eng: Engine) -> None:
    mine = place(eng, "a", "no", "buy", 5, 0.40)  # a rests an ask at YES 0.60
    place(eng, "b", "no", "buy", 5, 0.42)  # b rests an ask at YES 0.58
    res = place(eng, "a", "yes", "buy", 20, 0.65)
    assert res["quantityTraded"] == 5
    assert res["terminalReasonCode"] == "SelfTradePrevented"
    assert res["open"] is False
    assert eng.state.orders[mine["orderId"]].status == "open"


def test_no_stp_when_own_order_does_not_cross(eng: Engine) -> None:
    place(eng, "a", "no", "buy", 5, 0.30)  # a's ask at YES 0.70
    res = place(eng, "a", "yes", "buy", 5, 0.65)
    assert res["open"] is True
    assert res["terminalReasonCode"] is None


def test_gtd_orders_expire_at_read_and_match_time(eng: Engine) -> None:
    place(eng, "b", "no", "buy", 5, 0.40, expirationDate=iso(T0 + timedelta(seconds=10)))
    assert eng.book(EX, T0 + timedelta(seconds=5))["asks"] == [(6_000, 5)]
    later = T0 + timedelta(seconds=11)
    assert eng.book(EX, later)["asks"] == []
    res = place(eng, "a", "yes", "buy", 5, 0.60, at=later)
    assert res["quantityTraded"] == 0


def test_expiration_in_the_past_is_rejected(eng: Engine) -> None:
    with pytest.raises(EngineError):
        place(eng, "a", "yes", "buy", 1, 0.5, expirationDate=iso(T0 - timedelta(seconds=1)))


def test_insufficient_balance_counts_resting_reservations(eng: Engine) -> None:
    with pytest.raises(EngineError) as info:
        place(eng, "a", "yes", "buy", 2_000, 0.60)
    assert info.value.details["reason"] == "insufficient_balance"
    place(eng, "a", "yes", "buy", 1_000, 0.60)  # reserves 600
    with pytest.raises(EngineError):
        place(eng, "a", "no", "buy", 1_000, 0.45)  # would need 450 more
    place(eng, "a", "no", "buy", 800, 0.45)  # 360 fits in the remaining 400


def test_cancel_semantics(eng: Engine) -> None:
    res = place(eng, "a", "yes", "buy", 1, 0.5)
    with pytest.raises(EngineError) as info:
        eng.cancel("b", res["orderId"], T0)
    assert info.value.status == 403
    eng.cancel("a", res["orderId"], T0)
    with pytest.raises(EngineError) as info:
        eng.cancel("a", res["orderId"], T0)
    assert (info.value.status, info.value.code) == (409, "CONFLICT")
    with pytest.raises(EngineError) as info:
        eng.cancel("a", 424242, T0)
    assert info.value.status == 404


def test_snapshot_restore(eng: Engine) -> None:
    snap = eng.snapshot()
    place(eng, "a", "yes", "buy", 1, 0.5)
    eng.restore(snap)
    assert eng.open_orders("a", T0) == []
