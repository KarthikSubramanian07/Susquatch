"""Far quoter: rest tiny-priced orders far from fair on near-certain outcomes, and wait.

Day one showed where the leaderboard's money came from: accounts that paid 0.95 to 0.995 for ~1%
outcomes (and sold 99% outcomes near 0.005), with the whole balance, in the opening hours. Whoever
had resting orders at those prices was paid for them. The far quoter is the legitimate, passive
version of that: on every verified market whose fresh fair value is below ``max_fair`` it rests
BUY NO at NO prices ``levels`` (0.01 .. 0.10, i.e. selling YES at 0.99 .. 0.90), and on every
market whose fair is above ``1 - max_fair`` it rests BUY YES at those same prices. A fill hands us
a share worth ~0.99 for a few cents; the only cost of resting is the cash the platform reserves,
capped at ``reserve_frac`` of equity in total and ``max_market_frac`` per market.

Orders carry no ``input_ts``, so the OMS's stale-fair cancel leaves them alone; the quoter itself
cancels a market's orders when its fair value leaves the band. Everything goes through the gateway
(lease, kill switch, risk caps including the glitch cap) like every other strategy.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import TYPE_CHECKING

from susq.platform.prices import PIPS_PER_UNIT, to_pips

from susq_bridge.gateway import OrderOutcome, OrderSpec

if TYPE_CHECKING:
    from susq_bridge.config import FarQuoteConfig
    from susq_bridge.gateway import Gateway
    from susq_bridge.market_state import MarketState

__all__ = ["FarQuoteStats", "FarQuoter", "Quote"]

log = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class Quote:
    exchange_id: str
    side: str  # what we buy: "no" on a ~0 outcome, "yes" on a ~1 outcome
    price_pips: int  # side-relative
    quantity: int
    fair_pips: int


@dataclass
class FarQuoteStats:
    scans: int = 0
    placed: int = 0
    cancelled: int = 0
    blocked: dict[str, int] = field(default_factory=dict)


class FarQuoter:
    def __init__(
        self,
        gateway: Gateway,
        market: MarketState,
        config: FarQuoteConfig,
        *,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
    ) -> None:
        self.gateway = gateway
        self.market = market
        self.config = config
        self.clock = clock
        self.stats = FarQuoteStats()

    def _block(self, reason: str) -> None:
        self.stats.blocked[reason] = self.stats.blocked.get(reason, 0) + 1

    def side_for(self, fair_pips: int) -> str | None:
        """Which side to rest on, or None when the market is not near-certain."""
        edge = round(self.config.max_fair * PIPS_PER_UNIT)
        if fair_pips <= edge:
            return "no"
        if fair_pips >= PIPS_PER_UNIT - edge:
            return "yes"
        return None

    def in_band(self, exchange_id: str) -> tuple[str, int] | None:
        """(side, fair) when the market is verified, fresh and near-certain."""
        check = self.market.fair_check(exchange_id)
        if not check.ok or check.fair_pips is None:
            self._block(check.reason)
            return None
        side = self.side_for(check.fair_pips)
        return None if side is None else (side, check.fair_pips)

    def fair_unavailable(self, exchange_id: str) -> bool:
        """True when fair is merely missing or stale (keep what is resting), not wrong or out of band."""
        return self.market.fair_check(exchange_id).reason in ("stale", "no_fair", "no_cup_price")

    def wanted(self, exchange_id: str, *, markets_in_band: int = 1) -> list[Quote]:
        """The resting ladder this market should have right now (empty when out of band).

        The reserve budget is spread evenly over every market in the band (thin everywhere beats
        deep on the first few), then capped per market.
        """
        band = self.in_band(exchange_id)
        if band is None:
            return []
        side, fair_pips = band
        equity = self.gateway.risk.equity
        spread = self.config.reserve_frac * equity / max(1, markets_in_band)
        market_budget = min(self.config.max_market_frac * equity, spread)
        per_level = market_budget / max(1, len(self.config.levels))
        quotes = []
        for price in self.config.levels:
            pips = to_pips(price)
            qty = int(per_level * PIPS_PER_UNIT // pips)
            if qty >= 1:
                quotes.append(Quote(exchange_id, side, pips, qty, fair_pips))
        return quotes

    def _reserved(self) -> float:
        """Cash our resting far quotes tie up (SUSQies)."""
        if self.gateway.oms is None:
            return 0.0
        return sum(
            o.remaining * (o.yes_price if o.yes_dir > 0 else PIPS_PER_UNIT - o.yes_price) / PIPS_PER_UNIT
            for o in self.gateway.oms.open_orders(strategy="farquote")
        )

    async def scan(self) -> list[OrderOutcome]:
        """Top up every near-certain market's ladder; pull quotes from markets that left the band."""
        self.stats.scans += 1
        self.market.load_fair()
        await self.market.refresh_prices()
        if self.gateway.oms is None:
            return []
        outcomes: list[OrderOutcome] = []
        resting = self.gateway.oms.open_orders(strategy="farquote")
        by_market: dict[str, list] = {}
        for o in resting:
            by_market.setdefault(o.exchange_id, []).append(o)
        budget = self.config.reserve_frac * self.gateway.risk.equity
        reserved = self._reserved()
        in_band = sum(1 for ex in self.market.rules if self.in_band(ex) is not None)
        counts = {"inBand": in_band, "wanted": 0, "resting": 0, "placed": 0, "cancelled": 0}
        for exchange_id in sorted(self.market.rules):
            # A full pass places hundreds of orders at ~14 writes a minute, far longer than the fair
            # file's staleness window: re-read the file (cheap) so later markets are judged on a
            # fresh fair rather than all turning "stale" a minute into the scan.
            self.market.load_fair()
            quotes = self.wanted(exchange_id, markets_in_band=in_band)
            have = by_market.get(exchange_id, [])
            if not quotes:
                if self.fair_unavailable(exchange_id):
                    continue  # no verdict: leave the ladder resting
                for o in have:
                    if await self.gateway.cancel(o.order_id):
                        self.stats.cancelled += 1
                        counts["cancelled"] += 1
                continue
            for q in quotes:
                counts["wanted"] += 1
                yes_price = q.price_pips if q.side == "yes" else PIPS_PER_UNIT - q.price_pips
                if any(o.side == q.side and o.yes_price == yes_price for o in have):
                    counts["resting"] += 1
                    continue
                cost = q.quantity * q.price_pips / PIPS_PER_UNIT
                if reserved + cost > budget:
                    self._block("reserve_budget")
                    continue
                outcome = await self.place(q)
                outcomes.append(outcome)
                if outcome.placed:
                    reserved += cost
                    counts["placed"] += 1
        self.gateway.journal.event(
            "farquote_scan",
            {
                **counts,
                "reserved": round(reserved, 2),
                "budget": round(budget, 2),
                "equity": round(self.gateway.risk.equity, 2),
                "blocked": dict(self.stats.blocked),
            },
        )
        return outcomes

    async def place(self, q: Quote) -> OrderOutcome:
        spec = OrderSpec(
            exchange_id=q.exchange_id,
            side=q.side,  # type: ignore[arg-type]
            action="buy",
            quantity=q.quantity,
            price_pips=q.price_pips,
            ttl_s=self.config.order_ttl_s,
            # No fair_pips on purpose: these orders sit 85+ points through fair by design, and the glitch
            # cap (a per-account budget for taker fills against broken quotes) rejected the whole ladder.
            # The reserve and per-market budgets above are the far quoter's limits.
        )
        stamp = int(time.time())
        outcome = await self.gateway.place(
            f"farquote:{q.exchange_id}:{q.side}:{q.price_pips}:{stamp}", "farquote", spec
        )
        if outcome.placed:
            self.stats.placed += 1
        else:
            self._block(outcome.status)
        self.gateway.journal.event(
            "farquote_order",
            {
                "exchangeId": q.exchange_id,
                "side": q.side,
                "price": q.price_pips,
                "quantity": q.quantity,
                "fair": q.fair_pips,
                "status": outcome.status,
                "reason": outcome.reason,
            },
            idempotency_key=outcome.idempotency_key,
        )
        return outcome
