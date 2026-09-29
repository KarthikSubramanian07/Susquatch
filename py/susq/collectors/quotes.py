"""The normalized "reference mids" row every downstream consumer reads, whatever the venue."""

from __future__ import annotations

import math
from dataclasses import asdict, dataclass
from typing import Any

KALSHI, POLYMARKET = "kalshi", "polymarket"

# A two-sided quote wider than this is not a price, it's an absence of one; fall back to last trade.
MAX_USABLE_SPREAD = 0.20


@dataclass(frozen=True)
class RefQuote:
    """One real-money quote for "party X wins race Y", YES-space probabilities in [0, 1]."""

    venue: str
    race_id: str
    kind: str  # race_party | chamber
    party: str  # DEM | REP | IND
    venue_market_id: str  # Kalshi ticker, or Polymarket YES token id
    title: str
    bid: float | None
    ask: float | None
    last: float | None
    mid: float
    spread: float  # ask - bid, or MAX_USABLE_SPREAD when the mid came from a last trade
    volume: float
    ts_ns: int

    def as_row(self) -> dict[str, Any]:
        return asdict(self)


def blend_mid(bid: float | None, ask: float | None, last: float | None) -> tuple[float, float] | None:
    """Mid and effective spread from a top of book, falling back to the last trade.

    Returns None when there is no usable price at all. Venue "no bid" is 0 and "no ask" is 1,
    so both are treated as missing. A last trade is only trusted when it sits inside whatever
    side of the book exists (observed live: an Idaho longshot with a 0.1c ask and a stale 99.9c
    last print).
    """
    b = bid if bid is not None and 0.0 < bid < 1.0 else None
    a = ask if ask is not None and 0.0 < ask < 1.0 else None
    if b is not None and a is not None and b <= a and (a - b) <= MAX_USABLE_SPREAD:
        return (b + a) / 2.0, a - b
    if last is not None and 0.0 < last < 1.0 and (a is None or last <= a) and (b is None or last >= b):
        return last, MAX_USABLE_SPREAD
    if a is not None and b is None and a <= 0.02:
        return a / 2.0, a  # deep longshot: nothing but an ask at 0.5-2c
    if b is not None and a is None and b >= 0.98:
        return (1.0 + b) / 2.0, 1.0 - b  # deep favourite: nothing but a bid at 98c+
    return None


def to_float(value: Any) -> float | None:
    """Venue numbers arrive as strings, numbers or null; normalize to a finite float or None."""
    if value is None or value == "":
        return None
    try:
        out = float(value)
    except (TypeError, ValueError):
        return None
    return out if math.isfinite(out) else None
