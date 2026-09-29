"""ALL collateral in the bridge: the opt-in flag, buying power, the tracker, and racer sizing."""

from __future__ import annotations

import dataclasses
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

from susq.platform.models import (
    AllEconomics,
    Collateral,
    OrderRequest,
    PlaceOrderResult,
    Relationship,
)
from susq_bridge.collateral import CollateralTracker, request_legs
from susq_bridge.config import RiskConfig, load_config
from susq_bridge.gateway import _PendingView
from susq_bridge.risk import RiskEngine

T = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)
LOOSE = RiskConfig(
    per_market_gross_frac=10.0,
    total_gross_frac=10.0,
    glitch_through_fair_pts=15.0,
    glitch_max_frac=0.02,
    loss_per_hour_frac=0.03,
    reject_breaker_per_min=10,
    max_order_notional_frac=10.0,
)
ON = dataclasses.replace(LOOSE, use_collateral_buying_power=True)
PAIR = Relationship.model_validate(
    {
        "id": "rel-1",
        "type": "mutually_exclusive",
        "status": "active",
        "isExhaustive": True,
        "nodes": [{"exchangeId": "1", "role": "member"}, {"exchangeId": "2", "role": "member"}],
    }
)


@dataclass
class View:
    exchange_id: str
    yes_dir: int
    yes_price: int
    remaining: int


def req(ex: str, side: str, qty: int, price: int, action: str = "buy") -> OrderRequest:
    return OrderRequest(ex, side, action, qty, price, T + timedelta(minutes=5), "t")  # type: ignore[arg-type]


def risk(config: RiskConfig, *, cash: float | None, positions: dict[str, int] | None = None) -> RiskEngine:
    engine = RiskEngine(config, "t")
    engine.equity = 1_000_000.0
    engine.cash = cash
    engine.positions = dict(positions or {})
    tracker = CollateralTracker()
    tracker.set_relationships([PAIR])
    engine.cost_model = lambda reqs: tracker.effective_cost_pips(engine.positions, request_legs(reqs))
    return engine


# ── config ──────────────────────────────────────────────────────────────────────────────────


def test_flag_is_off_in_the_repo_config(home: Path) -> None:
    assert load_config(home, env={}).risk.use_collateral_buying_power is False


def test_flag_can_be_turned_on(home: Path) -> None:
    toml = home / "config" / "susq.toml"
    toml.write_text(
        toml.read_text().replace("use_collateral_buying_power = false", "use_collateral_buying_power = true")
    )
    assert load_config(home, env={}).risk.use_collateral_buying_power is True


# ── buying power (flag off: full notional) ──────────────────────────────────────────────────


def test_buying_power_caps_opening_shares_at_full_notional() -> None:
    d = risk(LOOSE, cash=10.0).check(req("7", "yes", 100, 5_000), [])
    assert (d.quantity, d.binding) == (20, ("buying_power",))


def test_unknown_cash_leaves_buying_power_to_the_platform() -> None:
    d = risk(LOOSE, cash=None).check(req("7", "yes", 100, 5_000), [])
    assert d.quantity == 100


def test_closing_shares_need_no_cash() -> None:
    d = risk(LOOSE, cash=0.0, positions={"7": -30}).check(req("7", "yes", 40, 5_000), [])
    assert d.quantity == 30


def test_resting_orders_reserve_cash_at_full_notional() -> None:
    d = risk(LOOSE, cash=10.0).check(req("7", "yes", 100, 5_000), [View("8", 1, 5_000, 10)])
    assert d.quantity == 10


def test_flag_off_ignores_the_advance_the_model_would_give() -> None:
    d = risk(LOOSE, cash=1.0, positions={"1": -10}).check(req("2", "no", 10, 4_500), [])
    assert d.quantity == 2  # 1.0 / 0.45: today's conservative rule


# ── buying power (flag on: modeled effective cost) ──────────────────────────────────────────


def test_flag_on_counts_only_the_modeled_effective_cost() -> None:
    engine = risk(ON, cash=1.0, positions={"1": -10})
    assert engine.uses_collateral
    d = engine.check(req("2", "no", 10, 4_500), [])
    assert d.quantity == 10  # NO on 1 already held: the floor backs the whole 4.5


def test_flag_on_without_a_model_falls_back_to_full_notional() -> None:
    engine = risk(ON, cash=1.0, positions={"1": -10})
    engine.cost_model = None
    assert not engine.uses_collateral
    assert engine.check(req("2", "no", 10, 4_500), []).quantity == 2


def test_bundle_legs_are_costed_together_in_order() -> None:
    leg1, leg2 = req("1", "no", 10, 6_000), req("2", "no", 10, 4_500)
    pending = [_PendingView(leg1)]
    on = risk(ON, cash=7.0).check(leg2, pending, prior_legs=[leg1])
    off = risk(LOOSE, cash=7.0).check(leg2, pending, prior_legs=[leg1])
    assert on.quantity == 10  # leg 1 costs 6.0 cash; leg 2 is fully advanced
    assert off.quantity == 2  # 7 - 6 reserved = 1.0 left: 2 shares at 0.45


# ── tracker ─────────────────────────────────────────────────────────────────────────────────


def test_tracker_records_engine_economics_and_outstanding() -> None:
    tracker = CollateralTracker()
    econ = AllEconomics(
        full_notional_cost=4.5,
        effective_entry_cost=0.0,
        collateral_savings=4.5,
        relationship_ids=["rel-1"],
        component_id="1-2",
        guaranteed_payout_floor_after=10.0,
        outstanding_advance_after=4.5,
        collateral_repayment=0.0,
        net_buying_power_impact=0.0,
    )
    res = PlaceOrderResult(exchange_id="2", side="no", quantity_traded=10, fill_price=0.45, all=econ)
    seen = tracker.observe("i-1", req("2", "no", 10, 4_500), res)
    assert seen is not None
    assert seen.journal()["collateralSavings"] == 4.5
    assert tracker.outstanding == {"1-2": 45_000}
    assert tracker.observe("i-2", req("9", "yes", 1, 5_000), PlaceOrderResult(exchange_id="9")) is None
    assert (tracker.responses, tracker.with_all) == (2, 1)
    tracker.observe_collateral(Collateral(data=[]))
    assert tracker.outstanding == {}


def test_tracker_without_relationships_charges_full_notional() -> None:
    tracker = CollateralTracker()
    legs = request_legs([req("1", "no", 10, 6_000), req("2", "no", 10, 4_500)])
    assert tracker.effective_cost_pips({}, legs) == 105_000
    tracker.set_relationships([PAIR])
    assert tracker.effective_cost_pips({}, legs) == 60_000
    tracker.outstanding = {"1-2": 80_000}  # most of the floor is already spoken for
    assert tracker.effective_cost_pips({"1": -10, "2": -10}, legs[1:]) == 25_000


# ── against the mock ────────────────────────────────────────────────────────────────────────


async def test_all_economics_are_recorded_even_with_the_flag_off(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("live")
    try:
        assert not bridge.gateway.risk.uses_collateral
        await harness.mock.post_admin("/_mock/violation", {"amount": 0.06})
        await bridge.racer.load_relationships()
        (outcome,) = await bridge.racer.poll_once()
        assert outcome.placed
        tracker = bridge.gateway.collateral
        assert tracker.with_all == 2
        assert tracker.savings_pips > 0  # the mock advanced the second NO leg
        assert tracker.outstanding
        journal = bridge.journal.path_for(harness.now().date()).read_text()
        assert journal.count("all_economics") == 2
    finally:
        await bridge.close()


async def test_paper_mode_gets_relationships_and_models_the_advance(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    bridge = await harness.open_bridge("paper")
    try:
        await harness.mock.post_admin("/_mock/violation", {"amount": 0.06})
        await bridge.racer.load_relationships()
        (outcome,) = await bridge.racer.poll_once()
        assert outcome.placed
        paper = bridge.gateway.paper
        assert paper is not None
        assert paper.collateral.outstanding  # same model as the mock, applied to paper fills
        assert bridge.gateway.collateral.savings_pips > 0
    finally:
        await bridge.close()


async def _racer_quantity(harness, *, collateral: bool) -> int:  # type: ignore[no-untyped-def]
    await harness.mock.post_admin("/_mock/reset", {})
    harness.config.intents_path.unlink(missing_ok=True)  # racer intent ids are per wall-clock second
    bridge = await harness.open_bridge("live")
    try:
        if collateral:
            bridge.gateway.risk.config = dataclasses.replace(
                bridge.gateway.risk.config, use_collateral_buying_power=True
            )
        # A budget small enough to bind: 0.05% of 100k = 50 SUSQies per bundle.
        bridge.racer.config = dataclasses.replace(bridge.racer.config, max_bundle_notional_frac=0.0005)
        await harness.mock.post_admin("/_mock/violation", {"amount": 0.06})
        await bridge.racer.load_relationships()
        (outcome,) = await bridge.racer.poll_once()
        assert outcome.placed
        return outcome.requests[0].quantity
    finally:
        await bridge.close()


async def test_racer_sizes_on_effective_cost_only_with_the_flag(harness) -> None:  # type: ignore[no-untyped-def]
    await harness.write_market_map()
    full = await _racer_quantity(harness, collateral=False)
    effective = await _racer_quantity(harness, collateral=True)
    assert full >= 1
    assert effective > full  # only the first leg consumes cash under the model


def test_tracker_compares_each_fill_with_the_model() -> None:
    tracker = CollateralTracker()
    tracker.set_relationships([PAIR])
    tracker.sync_positions({"1": -10})

    def response(savings: float) -> PlaceOrderResult:
        return PlaceOrderResult(
            exchange_id="2",
            side="no",
            action="buy",
            quantity_traded=10,
            fill_price=0.45,
            all=AllEconomics(
                full_notional_cost=4.5,
                effective_entry_cost=4.5 - savings,
                collateral_savings=savings,
                relationship_ids=["rel-1"],
                component_id="1-2",
                guaranteed_payout_floor_after=10.0,
                outstanding_advance_after=savings,
                collateral_repayment=0.0,
                net_buying_power_impact=4.5 - savings,
            ),
        )

    agree = tracker.observe("a", req("2", "no", 10, 5_500), response(4.5))
    assert agree is not None
    assert agree.mismatches(0.01) == []
    assert tracker.positions == {"1": -10, "2": -10}  # shadowed until the next reconcile

    tracker.sync_positions({"1": -10})
    tracker.outstanding = {}
    differ = tracker.observe("b", req("2", "no", 10, 5_500), response(0.0))  # engine gave nothing
    assert differ is not None
    problems = differ.mismatches(0.01)
    assert any("collateralSavings: engine 0.0000 vs model 4.5000" in p for p in problems)


def test_tracker_counts_linked_fills_without_a_quote() -> None:
    tracker = CollateralTracker()
    tracker.set_relationships([PAIR])
    res = PlaceOrderResult(exchange_id="2", side="no", action="buy", quantity_traded=3, fill_price=0.45)
    assert tracker.observe("c", req("2", "no", 3, 5_500), res) is None
    assert tracker.linked_without_all == 1
