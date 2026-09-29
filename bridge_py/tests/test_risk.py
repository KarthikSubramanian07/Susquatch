"""Risk engine: every cap, every breaker, and the property that no accepted order breaches a cap."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import UTC, datetime, timedelta

import pytest
from hypothesis import given, settings
from hypothesis import strategies as st
from susq.platform.models import OrderRequest
from susq_bridge.config import RiskConfig
from susq_bridge.risk import RiskEngine

T = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)
CFG = RiskConfig(
    per_market_gross_frac=0.05,
    total_gross_frac=0.60,
    glitch_through_fair_pts=15.0,
    glitch_max_frac=0.02,
    loss_per_hour_frac=0.03,
    reject_breaker_per_min=10,
    max_order_notional_frac=0.02,
    clusters={"national": 0.25, "region": 0.15, "state": 0.10, "house": 0.20},
)
CLUSTERS = {
    "1": ["national", "state:ME"],
    "2": ["national", "state:ME"],
    "3": ["national", "state:TX"],
    "4": ["house"],
}


@dataclass
class View:
    exchange_id: str
    yes_dir: int
    yes_price: int
    remaining: int


class Clock:
    def __init__(self) -> None:
        self.now = T

    def __call__(self) -> datetime:
        return self.now


def engine(equity: float = 100_000.0, **kw: object) -> RiskEngine:
    r = RiskEngine(CFG, "t", clusters=CLUSTERS, **kw)  # type: ignore[arg-type]
    r.equity = equity
    return r


def order(
    ex: str = "1", side: str = "yes", action: str = "buy", qty: int = 10, price: int = 5000
) -> OrderRequest:
    return OrderRequest(ex, side, action, qty, price, T + timedelta(minutes=5), "t")  # type: ignore[arg-type]


def test_tournament_mismatch_is_rejected() -> None:
    req = OrderRequest("1", "yes", "buy", 1, 5000, T + timedelta(minutes=5), "other")
    assert engine().check(req, []).reason == "tournament_mismatch"


def test_per_order_cap_shrinks() -> None:
    d = engine().check(order(qty=10_000, price=5000), [])
    assert d.accepted
    assert d.quantity == 4_000  # 2% of 100k = 2000 SUSQies at 0.50
    assert "per_order" in d.binding


def test_per_market_cap_counts_positions_and_resting_orders() -> None:
    r = engine()
    r.positions = {"1": 6_000}
    r.marks = {"1": 5_000}  # 3000 of value
    opens = [View("1", 1, 5_000, 2_000)]  # 1000 resting
    d = r.check(order(qty=10_000, price=5_000), opens)
    assert d.quantity == 2_000  # 5000 cap - 4000 used = 1000 -> 2000 shares at 0.50


def test_closing_orders_are_never_blocked_by_caps() -> None:
    r = engine()
    r.positions = {"1": 50_000}
    r.marks = {"1": 5_000}  # far beyond every cap already
    d = r.check(order(side="no", qty=5_000, price=5_000), [])
    assert d.accepted
    assert d.quantity == 5_000
    d = r.check(order(side="yes", qty=5, price=5_000), [])
    assert not d.accepted


def test_cluster_cap() -> None:
    r = engine()
    r.positions = {"1": 18_000}
    r.marks = {"1": 5_000}  # 9000 in state:ME (cap 10 000)
    d = r.check(order(ex="2", qty=10_000, price=5_000), [])
    assert d.quantity == 2_000
    assert "cluster:state:ME" in d.binding


def test_glitch_cap() -> None:
    r = engine()
    d = r.check(order(qty=100, price=3_000), [], fair_pips=5_000)  # 20 pts through fair
    assert d.glitch
    r.commit(order(qty=d.quantity, price=3_000), d)
    assert r.glitch_used_pips == d.quantity * 3_000
    r.glitch_used_pips = int(0.02 * 100_000 * 10_000)  # budget spent
    d = r.check(order(qty=100, price=3_000), [], fair_pips=5_000)
    assert not d.accepted
    assert "glitch" in d.binding
    assert r.check(order(qty=100, price=4_000), [], fair_pips=5_000).accepted  # 10 pts: not a glitch


def test_self_cross_guard() -> None:
    r = engine()
    opens = [View("1", -1, 6_000, 5)]  # our ask at YES 0.60
    assert r.check(order(price=6_000), opens).reason == "self_cross"
    assert r.check(order(price=5_950), opens).accepted
    assert r.check(order(side="no", price=3_500), opens).accepted  # same direction as our ask


def test_locked_legs_cannot_be_reduced() -> None:
    r = engine(locked={"1"})
    r.positions = {"1": 100}
    assert r.check(order(side="no", qty=10, price=4_000), []).reason == "locked_leg"
    assert r.check(order(side="yes", qty=1, price=4_000), []).accepted


def test_reject_breaker_trips_and_calls_back() -> None:
    clock = Clock()
    tripped: list[str] = []
    r = engine(clock=clock)
    r.on_trip = tripped.append
    for _ in range(10):
        r.record_reject()
    assert not r.frozen
    clock.now += timedelta(seconds=61)
    for _ in range(10):
        r.record_reject()
    assert not r.frozen  # the old rejects aged out
    r.record_reject()
    assert r.frozen
    assert tripped[0].startswith("reject_breaker")
    assert r.check(order(), []).reason.startswith("frozen")  # type: ignore[union-attr]


def test_loss_per_hour_breaker() -> None:
    clock = Clock()
    r = engine(clock=clock)
    r.observe_equity(100_000)
    clock.now += timedelta(minutes=30)
    r.observe_equity(97_500)
    assert not r.frozen
    clock.now += timedelta(minutes=10)
    r.observe_equity(96_900)
    assert r.frozen
    assert "loss_per_hour" in (r.frozen_reason or "")


def test_loss_window_is_one_hour() -> None:
    clock = Clock()
    r = engine(clock=clock)
    r.observe_equity(100_000)
    clock.now += timedelta(minutes=61)
    r.observe_equity(96_000)
    assert not r.frozen


def test_no_equity_no_trading() -> None:
    assert engine(equity=0).check(order(), []).reason == "no_equity"


# ── the property ────────────────────────────────────────────────────────────────────────────

EXCHANGES = ["1", "2", "3", "4"]
prices = st.integers(min_value=1, max_value=199).map(lambda t: t * 50)
views = st.builds(
    View,
    exchange_id=st.sampled_from(EXCHANGES),
    yes_dir=st.sampled_from([1, -1]),
    yes_price=prices,
    remaining=st.integers(min_value=1, max_value=5_000),
)
orders = st.builds(
    order,
    ex=st.sampled_from(EXCHANGES),
    side=st.sampled_from(["yes", "no"]),
    action=st.sampled_from(["buy", "sell"]),
    qty=st.integers(min_value=1, max_value=100_000),
    price=prices,
)


def _unit(req: OrderRequest) -> int:
    return req.yes_price_pips if req.yes_direction > 0 else 10_000 - req.yes_price_pips


@settings(max_examples=200, deadline=None)
@given(
    equity=st.floats(min_value=100, max_value=1_000_000),
    positions=st.dictionaries(st.sampled_from(EXCHANGES), st.integers(min_value=-20_000, max_value=20_000)),
    marks=st.dictionaries(st.sampled_from(EXCHANGES), prices),
    opens=st.lists(views, max_size=6),
    req=orders,
    fair=st.one_of(st.none(), prices),
    glitch_used=st.integers(min_value=0, max_value=10**9),
)
def test_no_accepted_order_ever_breaches_a_cap(
    equity: float,
    positions: dict[str, int],
    marks: dict[str, int],
    opens: list[View],
    req: OrderRequest,
    fair: int | None,
    glitch_used: int,
) -> None:
    r = engine(equity=equity)
    r.positions = {k: v for k, v in positions.items() if v}
    r.marks = marks
    r.glitch_used_pips = glitch_used
    d = r.check(req, opens, fair_pips=fair)
    if not d.accepted:
        return
    assert 1 <= d.quantity <= req.quantity
    unit = _unit(req)
    opened = max(0, d.quantity - r.closing_shares(req)) * unit
    eq = equity * 10_000
    exchanges = set(r.positions) | {o.exchange_id for o in opens} | {req.exchange_id}

    def exposure(ex: str) -> int:
        return r.market_exposure_pips(ex, opens)

    def within(cap: float, used: float) -> bool:
        return opened <= max(0.0, cap - used) + 1e-6

    assert within(CFG.max_order_notional_frac * eq, 0)
    assert within(CFG.per_market_gross_frac * eq, exposure(req.exchange_id))
    assert within(CFG.total_gross_frac * eq, sum(exposure(e) for e in exchanges))
    for cluster in CLUSTERS.get(req.exchange_id, []):
        frac = CFG.clusters[cluster.split(":")[0]]
        members = [e for e in exchanges if cluster in CLUSTERS.get(e, [])]
        assert within(frac * eq, sum(exposure(e) for e in members))
    if d.glitch:
        assert within(CFG.glitch_max_frac * eq, glitch_used)


@pytest.mark.parametrize("price", [50, 9950])
def test_extreme_prices_are_handled(price: int) -> None:
    assert engine().check(order(price=price), []).accepted
