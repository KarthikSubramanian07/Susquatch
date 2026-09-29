"""Cash + FIFO-lot position accounting in the platform's single YES-space book.

Shared by the mock exchange and the paper broker so both settle trades by exactly the same rules:

* A fill is described in YES space: ``yes_qty`` shares (``+`` = the account bought YES exposure,
  ``-`` = it sold YES exposure) at YES price ``x`` pips.
* Opening YES costs ``x`` per share. Opening NO costs ``1 - x`` per share.
* Buying the side opposite to a held position **nets FIFO and pays 1 per cancelled pair**
  immediately. Economically that is identical to selling the held side: closing YES receives ``x``,
  closing NO receives ``1 - x``.

All money is held as integer **pip-units** (1 SUSQie = 10 000 pip-units), so ledgers never drift.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field
from datetime import datetime

from susq.platform.prices import PIPS_PER_UNIT

__all__ = ["Execution", "Holding", "Ledger", "Lot", "opening_cost_pips", "units"]


def units(pip_units: int) -> float:
    """Pip-units -> SUSQies."""
    return pip_units / PIPS_PER_UNIT


def opening_cost_pips(yes_dir: int, yes_price_pips: int, shares: int) -> int:
    """Cash needed to open ``shares`` in direction ``yes_dir`` (+1 YES / -1 NO) at a YES price."""
    per_share = yes_price_pips if yes_dir > 0 else PIPS_PER_UNIT - yes_price_pips
    return per_share * shares


@dataclass(slots=True)
class Lot:
    lot_id: str
    side: str  # "yes" | "no"
    quantity: int
    entry_pips: int  # side-relative price paid per share
    opened_at: datetime


@dataclass(slots=True)
class Holding:
    exchange_id: str
    lots: deque[Lot] = field(default_factory=deque)
    realized_pips: int = 0  # "moneyEarned": realized cash from prior closes

    @property
    def quantity(self) -> int:
        """Signed: +YES / -NO (all open lots share one side)."""
        total = sum(lot.quantity for lot in self.lots)
        if not self.lots:
            return 0
        return total if self.lots[0].side == "yes" else -total

    @property
    def cost_basis_pips(self) -> int:
        return sum(lot.quantity * lot.entry_pips for lot in self.lots)

    def market_value_pips(self, yes_mark_pips: int) -> int:
        qty = self.quantity
        return qty * yes_mark_pips if qty >= 0 else -qty * (PIPS_PER_UNIT - yes_mark_pips)


@dataclass(frozen=True, slots=True)
class Execution:
    """What one fill did to an account."""

    cash_delta_pips: int
    closed: int
    opened: int
    realized_pips: int


class Ledger:
    """One account's cash and positions within one tournament."""

    def __init__(self, cash_pips: int) -> None:
        self.cash_pips = cash_pips
        self.holdings: dict[str, Holding] = {}
        self._next_lot = 1

    @property
    def cash(self) -> float:
        return units(self.cash_pips)

    def position(self, exchange_id: str) -> int:
        holding = self.holdings.get(exchange_id)
        return holding.quantity if holding else 0

    def closable(self, exchange_id: str, yes_dir: int) -> int:
        """Shares a fill in ``yes_dir`` would close before opening anything."""
        pos = self.position(exchange_id)
        return abs(pos) if pos * yes_dir < 0 else 0

    def opening_cost_pips(self, exchange_id: str, yes_dir: int, yes_price_pips: int, shares: int) -> int:
        """Cash a fill of ``shares`` would consume right now (closing shares consume nothing)."""
        opened = max(0, shares - self.closable(exchange_id, yes_dir))
        return opening_cost_pips(yes_dir, yes_price_pips, opened)

    def execute(self, exchange_id: str, yes_qty: int, yes_price_pips: int, at: datetime) -> Execution:
        """Apply a fill: FIFO-close the opposite side first, then open the remainder."""
        if yes_qty == 0:
            return Execution(0, 0, 0, 0)
        holding = self.holdings.setdefault(exchange_id, Holding(exchange_id))
        yes_dir = 1 if yes_qty > 0 else -1
        remaining = abs(yes_qty)
        cash_delta = 0
        realized = 0
        closed = 0

        # Closing leg: proceeds are x per YES share closed, (1 - x) per NO share closed.
        close_proceeds = yes_price_pips if yes_dir < 0 else PIPS_PER_UNIT - yes_price_pips
        while remaining and holding.lots and (holding.quantity * yes_dir) < 0:
            lot = holding.lots[0]
            n = min(remaining, lot.quantity)
            cash_delta += n * close_proceeds
            realized += n * (close_proceeds - lot.entry_pips)
            lot.quantity -= n
            remaining -= n
            closed += n
            if lot.quantity == 0:
                holding.lots.popleft()

        opened = remaining
        if opened:
            side = "yes" if yes_dir > 0 else "no"
            entry = yes_price_pips if yes_dir > 0 else PIPS_PER_UNIT - yes_price_pips
            cash_delta -= opened * entry
            holding.lots.append(Lot(f"lot-{self._next_lot}", side, opened, entry, at))
            self._next_lot += 1

        holding.realized_pips += realized
        self.cash_pips += cash_delta
        return Execution(cash_delta, closed, opened, realized)

    def holdings_value_pips(self, marks: dict[str, int]) -> int:
        """Mark-to-market value of all non-flat holdings; missing marks value at cost."""
        total = 0
        for ex, holding in self.holdings.items():
            if holding.quantity == 0:
                continue
            mark = marks.get(ex)
            total += holding.cost_basis_pips if mark is None else holding.market_value_pips(mark)
        return total

    def account_value_pips(self, marks: dict[str, int]) -> int:
        return self.cash_pips + self.holdings_value_pips(marks)
