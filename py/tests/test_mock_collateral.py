"""ALL collateral in the mock exchange: order responses, reads, transactions, settlement, the switch."""

from __future__ import annotations

import itertools
import random
from datetime import UTC, datetime
from typing import Any

import pytest
from hypothesis import HealthCheck, given, settings
from hypothesis import strategies as st
from susq.mock.engine import Account, Engine, EngineError, MarketDef
from susq.mock.server import MockConfig, config_from_args
from susq.mock.testing import T0, MockEnv, open_mock_env
from susq.platform.collateral import Relation, payout_pips
from susq.platform.errors import ApiError
from susq.platform.ledger import Ledger
from susq.platform.prices import PIPS_PER_UNIT

U = PIPS_PER_UNIT


def senate_pair(env: MockEnv) -> tuple[str, str, str]:
    rel = next(r for r in env.mx.scenario.relationships if r.type == "mutually_exclusive" and r.is_exhaustive)
    dem, rep = rel.exchange_ids
    return rel.id, dem, rep


def profile(env: MockEnv) -> str:
    return env.mx.scenario.keys["mock-key-alpha"]


async def no_bundle(env: MockEnv, dem: str, rep: str, qty: int = 10, key: str = "bundle-1") -> Any:
    """Buy NO on both legs, limit exactly at the best YES bid (so the reservation is the real cost)."""
    legs = []
    for ex in (dem, rep):
        bid, _ = env.mx.engine.best(ex, env.mx.clock.now())
        assert bid is not None
        legs.append(env.order(ex, "no", "buy", qty, U - bid))
    return await env.client.place_multi_leg(legs, key)


# ── HTTP ────────────────────────────────────────────────────────────────────────────────────


async def test_second_leg_of_a_no_bundle_is_backed_and_reported() -> None:
    async with open_mock_env() as env:
        rel_id, dem, rep = senate_pair(env)
        cash0 = env.mx.engine.account(profile(env)).ledger.cash_pips
        res = await no_bundle(env, dem, rep)
        first, second = (item.data for item in sorted(res.results, key=lambda i: i.index))
        assert first.all is not None
        assert second.all is not None
        assert first.all.collateral_savings == 0
        assert first.all.relationship_ids == [rel_id]
        assert first.all.component_id == f"{dem}-{rep}"
        assert second.all.full_notional_cost == second.total_cost
        assert second.all.collateral_savings == second.total_cost  # floor 10 covers it all
        assert second.all.effective_entry_cost == 0
        assert second.all.guaranteed_payout_floor_after == 10
        assert second.all.outstanding_advance_after == second.total_cost
        assert second.all.net_buying_power_impact == 0
        cash1 = env.mx.engine.account(profile(env)).ledger.cash_pips
        assert cash0 - cash1 == round(first.total_cost * U)  # only the first leg took cash

        coll = await env.client.get_collateral(env.tid)
        (row,) = coll.data
        assert row.component_id == f"{dem}-{rep}"
        assert row.covered_exchange_ids == [dem, rep]
        assert row.guaranteed_payout_floor == 10
        assert row.outstanding_advance == second.total_cost
        assert row.profile_id == profile(env)

        txs = await env.client.get_transactions()
        advances = [t for t in txs.data if t.transaction_type == "ALL_COLLATERAL_ADVANCE"]
        assert len(advances) == 1
        assert advances[0].amount == second.total_cost
        assert advances[0].component_id == row.component_id
        assert advances[0].is_collateral


async def test_unlinked_exchange_has_no_relationship_quote() -> None:
    async with open_mock_env() as env:
        linked = {e for r in env.mx.scenario.relationships for e in r.exchange_ids}
        house = next(ex for ex in sorted(env.mx.engine.by_exchange) if ex not in linked)
        res = await env.client.place_order(env.order(house, "yes", "buy", 5, 9_900), "house-1")
        assert res.quantity_traded == 5
        assert res.all is None
        assert (await env.client.get_collateral(env.tid)).data == []


async def test_selling_the_backing_leg_repays_the_advance() -> None:
    async with open_mock_env() as env:
        _, dem, rep = senate_pair(env)
        res = await no_bundle(env, dem, rep)
        advanced = res.results[1].data.all
        assert advanced is not None
        sell = await env.client.place_order(env.order(rep, "no", "sell", 10, 100), "sell-1")
        assert sell.action == "sell"  # fully backed: stays a sell
        assert sell.all is not None
        assert sell.all.collateral_repayment == advanced.outstanding_advance_after
        assert sell.all.outstanding_advance_after == 0
        assert sell.all.guaranteed_payout_floor_after == 0
        txs = await env.client.get_transactions(types="trade")
        assert "ALL_COLLATERAL_REPAYMENT" in {t.transaction_type for t in txs.data}
        (row,) = (await env.client.get_collateral(env.tid)).data  # the NO on dem still touches it
        assert (row.outstanding_advance, row.guaranteed_payout_floor) == (0, 0)


async def test_settlement_releases_everything() -> None:
    async with open_mock_env() as env:
        _, dem, rep = senate_pair(env)
        await no_bundle(env, dem, rep)
        pid = profile(env)
        await env.post_admin("/_mock/settle", {"exchangeId": dem, "outcome": "YES"})
        acct = env.mx.engine.account(pid)
        assert acct.collateral.outstanding  # the NO on rep still backs it (floor 10 >= advance)
        out = await env.post_admin("/_mock/settle", {"exchangeId": rep, "outcome": "NO"})
        mine = next(s for s in out["settled"] if s["profileId"] == pid)
        assert mine["payout"] == 10
        assert mine["released"] > 0
        assert acct.collateral.outstanding == {}
        assert acct.ledger.cash_pips >= 0
        assert (await env.client.get_collateral(env.tid)).data == []
        txs = await env.client.get_transactions(types="settlement")
        assert "ALL_COLLATERAL_RELEASE" in {t.transaction_type for t in txs.data}
        settlements = await env.client.get_settlements("midterm-elections")
        assert {s.exchange_id for s in settlements.data} == {dem, rep}


async def test_contradictory_settlement_is_refused() -> None:
    async with open_mock_env() as env:
        _, dem, rep = senate_pair(env)
        await env.post_admin("/_mock/settle", {"exchangeId": dem, "outcome": "YES"})
        resp = await env.admin.post("/_mock/settle", json={"exchangeId": rep, "outcome": "YES"})
        assert resp.status_code == 400
        assert "contradicts" in resp.json()["error"]["message"]


async def test_collateral_buys_power_the_cash_alone_would_not() -> None:
    """With 7 SUSQies the NO bundle (about 10.1 nominal) only fits thanks to the advance."""
    async with open_mock_env() as env:
        _, dem, rep = senate_pair(env)
        env.mx.engine.account(profile(env)).ledger.cash_pips = 7 * U
        res = await no_bundle(env, dem, rep)
        assert all(item.data.quantity_traded == 10 for item in res.results)
        assert env.mx.engine.account(profile(env)).ledger.cash_pips >= 0


async def test_without_collateral_the_same_bundle_is_short_of_cash() -> None:
    async with open_mock_env(MockConfig(seed=7, frozen_now=T0, collateral=False)) as env:
        _, dem, rep = senate_pair(env)
        env.mx.engine.account(profile(env)).ledger.cash_pips = 7 * U
        with pytest.raises(ApiError) as caught:
            await no_bundle(env, dem, rep)
        assert caught.value.details.get("reason") == "insufficient_balance"


async def test_no_collateral_switch_restores_plain_cash() -> None:
    async with open_mock_env(MockConfig(seed=7, frozen_now=T0, collateral=False)) as env:
        _, dem, rep = senate_pair(env)
        cash0 = env.mx.engine.account(profile(env)).ledger.cash_pips
        res = await no_bundle(env, dem, rep)
        assert all(item.data.all is None for item in res.results)
        spent = sum(item.data.total_cost for item in res.results)
        assert cash0 - env.mx.engine.account(profile(env)).ledger.cash_pips == round(spent * U)
        assert (await env.client.get_collateral(env.tid)).data == []
        assert (await env.state())["collateral"] is False


def test_cli_flag() -> None:
    assert config_from_args([]).collateral is True
    assert config_from_args(["--no-collateral"]).collateral is False


async def test_tournament_transactions_omit_collateral_detail() -> None:
    async with open_mock_env() as env:
        _, dem, rep = senate_pair(env)
        await no_bundle(env, dem, rep)
        resp = await env.admin.get(
            "/api/v1/tournaments/midterm-elections/portfolio/transactions",
            headers={"Authorization": "Bearer mock-key-alpha"},
        )
        rows = resp.json()["data"]
        assert any(r["transactionType"] == "ALL_COLLATERAL_ADVANCE" for r in rows)
        assert all("componentId" not in r for r in rows)


async def test_collateral_changes_go_out_once_on_the_user_channel() -> None:
    async with open_mock_env() as env:
        _, dem, rep = senate_pair(env)
        pid = profile(env)
        assert env.mx.take_collateral_changes(pid) == []
        env.mx.engine.account(pid).collateral.events.clear()
        await no_bundle(env, dem, rep)
        # publish() already drained what the bundle produced; nothing is sent twice.
        assert env.mx.take_collateral_changes(pid) == []
        assert env.mx.collateral_published[pid] == len(env.mx.engine.account(pid).collateral.events) == 1


# ── engine properties ───────────────────────────────────────────────────────────────────────

A, B, C = "11", "12", "13"
RELATIONS = [
    Relation("me", "mutually_exclusive", (A, B), is_exhaustive=True),
    Relation("imp", "implication", (), antecedent=C, consequent=A),
]


def small_engine() -> Engine:
    markets = [MarketDef(str(i), ex, f"m{i}") for i, ex in enumerate((A, B, C), start=1)]
    eng = Engine("t", markets, RELATIONS)
    eng.add_account(Account("mm", "mm", Ledger(10**7 * U), is_bot=True))
    eng.add_account(Account("me", "me", Ledger(25 * U)))
    return eng


def body(ex: str, side: str, action: str, qty: int, tick: int) -> dict[str, Any]:
    return {
        "exchangeId": ex,
        "side": side,
        "action": action,
        "quantity": qty,
        "price": tick * 50 / U,
        "tournamentId": "t",
    }


def allowed_outcomes() -> list[dict[str, int]]:
    out = []
    for bits in itertools.product((0, 1), repeat=3):
        outcome = dict(zip((A, B, C), bits, strict=True))
        if all(r.allows(outcome) for r in RELATIONS):
            out.append(outcome)
    return out


def check_invariants(eng: Engine) -> None:
    for acct in eng.state.accounts.values():
        assert acct.ledger.cash_pips >= 0, acct.username
        for comp in eng.graph.components.values():
            positions = {ex: acct.ledger.position(ex) for ex in comp.exchange_ids}
            floor = acct.collateral.floor_pips(comp, acct.ledger)
            held = acct.collateral.outstanding.get(comp.id, 0)
            assert 0 <= held <= (floor or 0)
            remaining = [
                o for o in allowed_outcomes() if all(o[ex] == v for ex, v in eng.graph.settled.items())
            ]
            payouts = [
                payout_pips(positions, comp.exchange_ids, tuple(o[ex] for ex in comp.exchange_ids))
                for o in remaining
            ]
            if floor is not None:
                assert all(floor <= p for p in payouts)  # a true lower bound


orders = st.tuples(
    st.sampled_from((A, B, C)),
    st.sampled_from(("yes", "no")),
    st.sampled_from(("buy", "sell")),
    st.integers(1, 40),
    st.integers(1, 199),
)


@settings(max_examples=200, deadline=None, suppress_health_check=[HealthCheck.too_slow])
@given(
    ops=st.lists(st.tuples(st.sampled_from(("mm", "me", "me")), orders), min_size=1, max_size=40),
    seed=st.integers(0, 10_000),
)
def test_engine_invariants_hold_through_trading_and_settlement(
    ops: list[tuple[str, tuple[str, str, str, int, int]]], seed: int
) -> None:
    """Interleaved: our orders fill as takers AND rest to be hit as makers by the market maker."""
    eng = small_engine()
    now = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)
    for actor, (ex, side, action, qty, tick) in ops:
        reason = None
        try:
            eng.place(actor, body(ex, side, action, qty * (10 if actor == "mm" else 1), tick), now)
        except EngineError as err:
            reason = err.details.get("reason")
        assert reason in (None, "insufficient_balance")
        check_invariants(eng)
    rng = random.Random(seed)
    order = [A, B, C]
    rng.shuffle(order)
    for ex in order:
        choices = [o for o in ("YES", "NO") if eng.graph.consistent(ex, int(o == "YES"))]
        eng.settle(ex, rng.choice(choices), now)
        check_invariants(eng)
    for acct in eng.state.accounts.values():
        assert acct.collateral.outstanding == {}, acct.username
        assert all(h.quantity == 0 for h in acct.ledger.holdings.values())
