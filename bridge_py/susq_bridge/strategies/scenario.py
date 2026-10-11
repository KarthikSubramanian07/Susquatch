"""Scenario executor (EXP-008): work the live account toward the research scenario book in tranches.

A behavioural port of rust-live's ``crates/strategies/src/scenario_executor.rs``;
``tests/fixtures/scenario_tranches.json`` holds both to the same tranches.

For each target row, ``gap = target_qty - position`` (signed, +YES / -NO). Each decision sends one
tranche of ``clamp(ceil(|gap| x tranche_frac), min_tranche_qty, |gap|)`` shares, always as a buy: YES
when the gap is positive, NO when it is negative (buying NO reduces a YES holding and vice versa). A
tranche that would flip the position through zero stops at flat; the other side opens on a later pass.

Guards:

* a tranche that increases ``|position|`` is skipped while the target file is stale or invalid;
  reductions still proceed;
* a ``locked`` leg is never reduced;
* only verified markets, only fresh (uncrossed) books, and nothing on an exchange where a scenario
  order is already working.

Pacing: nothing before ``start_utc``, at least ``min_interval_s`` between tranches on one exchange, and
no tranche once the notional emitted in the trailing hour exceeds ``max_hourly_notional_frac`` of
equity. The executor remembers what it emitted for the trailing hour; a restart forgets that, which
only ever errs toward sending a tranche earlier.

Price cap: ``max_entry_price`` is a YES-space cap. A YES buy is limited to
``min(best ask, floor_tick(max_entry_price))``; a NO buy to ``min(best NO ask, floor_tick(1 -
max_entry_price))``. A cap with no valid price means no order.

Per-order cap: each tranche is clamped so ``qty x limit <= [risk] max_order_notional_frac x equity``,
so risk never refuses the same oversized tranche forever; when one share is already over the cap,
nothing is sent and the pass says so. Everything then goes through the gateway (lease, kill switch,
time gate, risk caps, rate budget) like every other strategy.
"""

from __future__ import annotations

import logging
import math
from collections.abc import Awaitable, Callable, Collection, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING, Literal, Protocol

from susq.platform.prices import PIPS_PER_UNIT, to_pips

from susq_bridge.gateway import OrderOutcome, OrderSpec

if TYPE_CHECKING:
    from susq_bridge.config import ScenarioConfig
    from susq_bridge.gateway import Gateway
    from susq_bridge.market_state import MarketState
    from susq_bridge.targets import TargetsReader

__all__ = [
    "NAME",
    "ScenarioDecision",
    "ScenarioExecutor",
    "ScenarioOrder",
    "ScenarioPlanner",
    "ScenarioStats",
    "ScenarioView",
    "Tranche",
    "floor_from_prob",
]

log = logging.getLogger(__name__)

#: Strategy name on orders and intents (the intent id prefix).
NAME = "scenario"
NS_PER_S = 1_000_000_000
HOUR_NS = 3600 * NS_PER_S
TICK_PIPS = 50
MIN_PIPS = TICK_PIPS
MAX_PIPS = PIPS_PER_UNIT - TICK_PIPS
#: A book older than this is re-read before pricing a tranche (rust-live: ``[live] book_max_age_s``).
BOOK_MAX_AGE_S = 2.0
_EPOCH = datetime(1970, 1, 1, tzinfo=UTC)

Side = Literal["yes", "no"]
#: YES-space ``(best bid, best ask)`` pips (None = empty side); None for "no fresh book".
BookTop = tuple[int | None, int | None]
BookFn = Callable[[str], Awaitable[BookTop | None]]


def floor_from_prob(p: float) -> int | None:
    """Largest valid price ``<= p`` in pips (rust-live ``Pips::floor_from_prob``), if any."""
    if not math.isfinite(p):
        return None
    raw: int = math.floor(p * PIPS_PER_UNIT + 1e-6)
    if raw < MIN_PIPS:
        return None
    raw = min(raw, MAX_PIPS)
    return raw - raw % TICK_PIPS


def to_ns(ts: datetime) -> int:
    """Exact nanoseconds since the epoch (microsecond resolution)."""
    return (ts - _EPOCH) // timedelta(microseconds=1) * 1_000


class TargetRow(Protocol):
    """``susq.v1.TargetPosition`` (or anything shaped like it)."""

    @property
    def exchange_id(self) -> str: ...
    @property
    def target_qty(self) -> int: ...
    @property
    def max_entry_price(self) -> float: ...
    @property
    def locked(self) -> bool: ...


@dataclass(frozen=True, slots=True)
class Tranche:
    side: Side
    qty: int
    #: Whether the step reduces ``|position|`` (otherwise it increases it).
    reduces: bool


@dataclass(frozen=True, slots=True)
class ScenarioOrder:
    exchange_id: str
    side: Side
    quantity: int
    price_pips: int  # side-relative limit
    target_qty: int
    position: int
    max_entry_price: float
    reduces: bool

    @property
    def notional(self) -> float:
        """Shares x price in the bought side's terms (rust-live ``OrderSpec::max_notional``)."""
        return float(self.price_pips / PIPS_PER_UNIT * self.quantity)


@dataclass(frozen=True, slots=True)
class ScenarioView:
    """Everything one decision reads (books come lazily through a :data:`BookFn`)."""

    now_ns: int
    equity: float
    #: Rows of the last good target file; None when none was ever loaded.
    targets: Sequence[TargetRow] | None
    #: False while the target file is stale or invalid: only reductions go out.
    increase_allowed: bool
    verified: Collection[str]
    positions: Mapping[str, int]
    #: Exchanges where a scenario order is already working.
    live: Collection[str]


@dataclass
class ScenarioDecision:
    orders: list[ScenarioOrder] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)


class ScenarioPlanner:
    """The rule plus its trailing-hour memory; no platform I/O (books come from the caller)."""

    def __init__(self, config: ScenarioConfig, *, max_order_notional_frac: float | None = None) -> None:
        self.config = config
        #: ``[risk] max_order_notional_frac``; None = no per-order clamp.
        self.max_order_notional_frac = max_order_notional_frac
        #: (emitted at ns, exchange, notional) for the trailing hour.
        self.sent: list[tuple[int, str, float]] = []

    def tranche(self, position: int, target: int) -> Tranche | None:
        """Next tranche from ``position`` toward ``target``, or None when already there."""
        gap = target - position
        if gap == 0:
            return None
        abs_gap = abs(gap)
        raw = math.ceil(abs_gap * max(self.config.tranche_frac, 0.0))
        qty = max(min(max(raw, self.config.min_tranche_qty), abs_gap), 1)
        reduces = position != 0 and (gap > 0) != (position > 0)
        if reduces:
            qty = min(qty, abs(position))
        return Tranche("yes" if gap > 0 else "no", qty, reduces)

    def _prune(self, now_ns: int) -> float:
        self.sent = [s for s in self.sent if now_ns - s[0] < HOUR_NS]
        return sum(s[2] for s in self.sent)

    def _too_soon(self, now_ns: int, exchange_id: str) -> bool:
        min_ns = round(self.config.min_interval_s * NS_PER_S)
        last = max((at for at, ex, _ in self.sent if ex == exchange_id), default=None)
        return min_ns > 0 and last is not None and now_ns - last < min_ns

    def _paused(self, view: ScenarioView, hourly: float) -> str | None:
        start = self.config.start_utc
        if start is not None and view.now_ns < to_ns(start):
            return "scenario: before start_utc, not started"
        cap = self.config.max_hourly_notional_frac
        if cap < 1.0 and hourly > cap * max(view.equity, 0.0):
            return f"scenario: trailing-hour notional {hourly:.2f} over {cap} of equity, pausing"
        return None

    async def decide(self, view: ScenarioView, book: BookFn) -> ScenarioDecision:
        d = ScenarioDecision()
        if not self.config.enabled or view.targets is None:
            return d
        hourly = self._prune(view.now_ns)
        for row in view.targets:
            if (why := self._paused(view, hourly)) is not None:
                d.notes.append(why)
                break
            hourly += await self._work(view, row, book, d)
        return d

    async def _work(self, view: ScenarioView, row: TargetRow, book: BookFn, d: ScenarioDecision) -> float:
        ex = row.exchange_id
        if ex not in view.verified or ex in view.live:
            return 0.0
        position = int(view.positions.get(ex, 0))
        t = self.tranche(position, int(row.target_qty))
        if t is None:
            return 0.0
        if self._too_soon(view.now_ns, ex):
            d.notes.append(f"scenario {ex}: last tranche under {self.config.min_interval_s:g} s ago, waiting")
            return 0.0
        if t.reduces and row.locked:
            d.notes.append(f"scenario {ex}: locked leg, not reducing")
            return 0.0
        if not t.reduces and not view.increase_allowed:
            d.notes.append(f"scenario {ex}: targets stale/invalid: not increasing exposure")
            return 0.0
        top = await book(ex)
        if top is None:
            d.notes.append(f"scenario {ex}: no fresh book")
            return 0.0
        mep = float(row.max_entry_price)
        cap = floor_from_prob(mep if t.side == "yes" else 1.0 - mep)
        if cap is None:
            d.notes.append(f"scenario {ex}: no valid {t.side} price under max_entry_price {mep}")
            return 0.0
        bid, ask = top
        touch = ask if t.side == "yes" else (PIPS_PER_UNIT - bid if bid is not None else None)
        price = cap if touch is None else min(touch, cap)
        qty = t.qty
        if self.max_order_notional_frac is not None:
            budget = self.max_order_notional_frac * max(view.equity, 0.0)
            fits = _shares_for_budget(budget, price)
            if fits < 1:
                d.notes.append(
                    f"scenario {ex}: one share at {price} is over the per-order cap {budget:.2f}, skipping"
                )
                return 0.0
            qty = min(qty, fits)
        order = ScenarioOrder(ex, t.side, qty, price, int(row.target_qty), position, mep, t.reduces)
        d.notes.append(f"scenario {ex}: buy {t.side} x{qty} @ {price} toward {row.target_qty}")
        d.orders.append(order)
        self.sent.append((view.now_ns, ex, order.notional))
        return order.notional


def _shares_for_budget(budget: float, price_pips: int) -> int:
    """Largest whole-share quantity whose cost at ``price_pips`` stays within ``budget`` (rust-live
    ``draft::shares_for_budget``)."""
    per_share = price_pips / PIPS_PER_UNIT
    if not (math.isfinite(budget) and budget > 0 and per_share > 0):
        return 0
    shares: int = math.floor(budget / per_share + 1e-9)
    return shares


@dataclass
class ScenarioStats:
    scans: int = 0
    placed: int = 0
    blocked: dict[str, int] = field(default_factory=dict)


class ScenarioExecutor:
    """The bridge side: builds the view from the OMS, risk and market state, prices on fresh books
    read through the gateway's rate budget, and sends each tranche through ``Gateway.place``."""

    def __init__(
        self,
        gateway: Gateway,
        market: MarketState,
        config: ScenarioConfig,
        targets: TargetsReader,
        *,
        max_order_notional_frac: float | None = None,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
    ) -> None:
        self.gateway = gateway
        self.market = market
        self.targets = targets
        self.clock = clock
        self.planner = ScenarioPlanner(config, max_order_notional_frac=max_order_notional_frac)
        self.stats = ScenarioStats()
        self._last_notes: list[str] | None = None

    @property
    def config(self) -> ScenarioConfig:
        return self.planner.config

    @config.setter
    def config(self, value: ScenarioConfig) -> None:
        self.planner.config = value

    def _block(self, reason: str) -> None:
        self.stats.blocked[reason] = self.stats.blocked.get(reason, 0) + 1

    def live_exchanges(self, now_ns: int) -> set[str]:
        """Exchanges with a scenario order resting or an unresolved scenario intent still in force."""
        oms = self.gateway.oms
        live = {o.exchange_id for o in oms.open_orders(strategy=NAME)} if oms is not None else set()
        for intent in self.gateway.intents.unresolved():
            if intent.strategy == NAME and intent.expiration_ns > now_ns:
                live.add(intent.exchange_id)
        return live

    async def top(self, exchange_id: str) -> BookTop | None:
        """YES-space best bid/ask from a book at most ``BOOK_MAX_AGE_S`` old; None when it cannot be
        read or is crossed (rust-live only prices on fresh, uncrossed books)."""
        try:
            book = await self.market.book(exchange_id, max_age_s=BOOK_MAX_AGE_S)
        except Exception:
            log.exception("scenario: book read for %s failed", exchange_id)
            return None
        bid = max((to_pips(lv.price) for lv in book.bids if lv.quantity > 0), default=None)
        ask = min((to_pips(lv.price) for lv in book.asks if lv.quantity > 0), default=None)
        if bid is not None and ask is not None and bid >= ask:
            return None
        return bid, ask

    def view(self, now: datetime) -> ScenarioView:
        now_ns = to_ns(now)
        risk = self.gateway.risk
        good = self.targets.good
        return ScenarioView(
            now_ns=now_ns,
            equity=risk.equity,
            targets=list(good.positions) if good is not None else None,
            increase_allowed=self.targets.increase_allowed,
            verified={ex for ex in self.market.rules if self.market.verified_rule(ex) is not None},
            positions=dict(risk.positions),
            live=self.live_exchanges(now_ns),
        )

    async def scan(self) -> list[OrderOutcome]:
        """One pass: refresh the targets, decide, and send this pass's tranches."""
        self.stats.scans += 1
        if not self.config.enabled or self.gateway.oms is None:
            return []
        now = self.clock()
        self.targets.refresh(to_ns(now))
        # Locked legs also go to the risk engine, which then refuses any strategy's order that would
        # reduce one (rust-live's risk context carries them the same way).
        self.gateway.risk.locked = self.targets.locked()
        if (why := await self.gateway.gate()) is not None:
            self._block(why.split(":")[0])  # no lease / killed / frozen: nothing is decided
            return []
        decision = await self.planner.decide(self.view(now), self.top)
        self._journal_state(decision.notes)
        stamp = to_ns(now) // NS_PER_S
        return [await self.place(order, stamp) for order in decision.orders]

    def _journal_state(self, notes: list[str]) -> None:
        """Journal the pass's notes when they change (a standing pause is journaled once)."""
        if notes == self._last_notes:
            return
        self._last_notes = list(notes)
        self.gateway.journal.event(
            "scenario_pass",
            {
                "notes": notes,
                "increaseAllowed": self.targets.increase_allowed,
                "targetsRejection": self.targets.rejection,
                "targetsStale": self.targets.stale,
            },
        )

    async def place(self, order: ScenarioOrder, stamp: int) -> OrderOutcome:
        spec = OrderSpec(
            exchange_id=order.exchange_id,
            side=order.side,
            action="buy",
            quantity=order.quantity,
            price_pips=order.price_pips,
            ttl_s=self.config.order_ttl_s,
            # No fair_pips / input_ts: scenario legs are bought on the research book's own price cap,
            # not against the live fair (rust-live sends them with fair_yes = None as well).
        )
        outcome = await self.gateway.place(f"{NAME}:{order.exchange_id}:{stamp}", NAME, spec)
        if outcome.placed:
            self.stats.placed += 1
        else:
            self._block(outcome.status)
        self.gateway.journal.event(
            "scenario_order",
            {
                "exchangeId": order.exchange_id,
                "side": order.side,
                "quantity": order.quantity,
                "limit": order.price_pips,
                "position": order.position,
                "target": order.target_qty,
                "maxEntryPrice": order.max_entry_price,
                "reduces": order.reduces,
                "status": outcome.status,
                "reason": outcome.reason,
                "sentQuantity": outcome.request.quantity if outcome.request is not None else None,
            },
            idempotency_key=outcome.idempotency_key,
        )
        return outcome
