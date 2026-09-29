"""Integer price arithmetic for The Super Market.

The platform quotes YES-normalized decimal prices in [0, 1] on a 0.005 tick, with limit prices
restricted to [0.005, 0.995]. Floats are fine on the wire and terrible for arithmetic, so everything
inside Susquatch holds prices as integer **pips** (1 pip = 1e-4 of a share's payout):

    0.005 -> 50 pips (one tick)      0.145 -> 1450 pips      0.995 -> 9950 pips

NO is the complement of YES inside a single book: a NO price of ``p`` pips is a YES price of
``10000 - p`` pips.
"""

from __future__ import annotations

import math
from typing import NewType

Pips = NewType("Pips", int)

PIPS_PER_UNIT = 10_000
TICK_PIPS = 50
MIN_PIPS = 50
MAX_PIPS = 9_950

__all__ = [
    "MAX_PIPS",
    "MIN_PIPS",
    "PIPS_PER_UNIT",
    "TICK_PIPS",
    "Pips",
    "PriceError",
    "ceil_tick",
    "clamp_limit",
    "complement",
    "floor_tick",
    "is_on_tick",
    "is_valid_limit",
    "nearest_tick",
    "to_pips",
    "to_price",
    "validate_limit",
]


class PriceError(ValueError):
    """A price that cannot be represented or is not a valid limit price."""


def to_pips(price: float) -> Pips:
    """Convert a decimal price in [0, 1] to pips, rounding to the nearest pip.

    Rounding (not truncation) matters: ``0.145 * 10000`` is ``1450.0000000000002`` in binary floating
    point and ``0.29 * 10000`` is ``2899.9999999999995``.
    """
    if not math.isfinite(price):
        raise PriceError(f"price is not finite: {price!r}")
    pips = round(price * PIPS_PER_UNIT)
    if not 0 <= pips <= PIPS_PER_UNIT:
        raise PriceError(f"price outside [0, 1]: {price!r}")
    return Pips(pips)


def to_price(pips: int) -> float:
    """Convert pips to the decimal price sent on the wire (exact to 4 decimal places)."""
    return round(pips / PIPS_PER_UNIT, 4)


def complement(pips: int) -> Pips:
    """YES pips <-> NO pips within one book."""
    return Pips(PIPS_PER_UNIT - pips)


def floor_tick(pips: int) -> Pips:
    """Largest tick multiple <= pips."""
    return Pips((pips // TICK_PIPS) * TICK_PIPS)


def ceil_tick(pips: int) -> Pips:
    """Smallest tick multiple >= pips."""
    return Pips(-((-pips) // TICK_PIPS) * TICK_PIPS)


def nearest_tick(pips: int) -> Pips:
    """Nearest tick multiple; exact halves round up (away from zero for valid prices)."""
    return Pips(((pips + TICK_PIPS // 2) // TICK_PIPS) * TICK_PIPS)


def is_on_tick(pips: int) -> bool:
    return pips % TICK_PIPS == 0


def is_valid_limit(pips: int) -> bool:
    """True when the platform would accept this as a limit price."""
    return MIN_PIPS <= pips <= MAX_PIPS and is_on_tick(pips)


def validate_limit(pips: int) -> Pips:
    """Return ``pips`` unchanged, or raise :class:`PriceError` if the platform would reject it."""
    if not is_on_tick(pips):
        raise PriceError(f"{pips} pips is off the {TICK_PIPS}-pip tick")
    if not MIN_PIPS <= pips <= MAX_PIPS:
        raise PriceError(f"{pips} pips outside [{MIN_PIPS}, {MAX_PIPS}]")
    return Pips(pips)


def clamp_limit(pips: int) -> Pips:
    """Clamp into the valid limit range (callers tick-round first)."""
    return Pips(min(MAX_PIPS, max(MIN_PIPS, pips)))
