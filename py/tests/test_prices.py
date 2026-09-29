from __future__ import annotations

import pytest
from hypothesis import given
from hypothesis import strategies as st
from susq.platform.prices import (
    MAX_PIPS,
    MIN_PIPS,
    TICK_PIPS,
    PriceError,
    ceil_tick,
    clamp_limit,
    complement,
    floor_tick,
    is_on_tick,
    is_valid_limit,
    nearest_tick,
    to_pips,
    to_price,
    validate_limit,
)

pips = st.integers(min_value=0, max_value=10_000)
limits = st.integers(min_value=MIN_PIPS // TICK_PIPS, max_value=MAX_PIPS // TICK_PIPS).map(
    lambda t: t * TICK_PIPS
)


def test_constants_match_the_official_tick() -> None:
    assert (TICK_PIPS, MIN_PIPS, MAX_PIPS) == (50, 50, 9950)
    assert to_pips(0.005) == TICK_PIPS
    assert to_pips(0.995) == MAX_PIPS


@pytest.mark.parametrize(("price", "expected"), [(0.145, 1450), (0.29, 2900), (0.57, 5700), (1.0, 10_000)])
def test_to_pips_rounds_float_noise(price: float, expected: int) -> None:
    assert to_pips(price) == expected


@pytest.mark.parametrize("bad", [float("nan"), float("inf"), -0.01, 1.01])
def test_to_pips_rejects_unrepresentable(bad: float) -> None:
    with pytest.raises(PriceError):
        to_pips(bad)


@given(pips)
def test_roundtrip(p: int) -> None:
    assert to_pips(to_price(p)) == p


@given(pips)
def test_complement_is_an_involution(p: int) -> None:
    assert complement(complement(p)) == p
    assert p + complement(p) == 10_000


@given(limits)
def test_complement_of_a_valid_limit_is_valid(p: int) -> None:
    assert is_valid_limit(complement(p))


@given(pips)
def test_tick_rounding_brackets(p: int) -> None:
    lo, hi, near = floor_tick(p), ceil_tick(p), nearest_tick(p)
    assert lo <= p <= hi
    assert hi - lo in (0, TICK_PIPS)
    assert near in (lo, hi)
    assert abs(near - p) <= TICK_PIPS // 2
    assert all(is_on_tick(x) for x in (lo, hi, near))


@given(pips)
def test_rounding_is_idempotent(p: int) -> None:
    for f in (floor_tick, ceil_tick, nearest_tick):
        assert f(f(p)) == f(p)


@given(st.integers(min_value=-5_000, max_value=15_000))
def test_clamp_limit_after_rounding_is_always_valid(p: int) -> None:
    assert is_valid_limit(clamp_limit(nearest_tick(p)))


@given(pips)
def test_validate_limit_agrees_with_is_valid(p: int) -> None:
    if is_valid_limit(p):
        assert validate_limit(p) == p
    else:
        with pytest.raises(PriceError):
            validate_limit(p)


@pytest.mark.parametrize("p", [0, 25, 49, 9951, 9999, 10_000, 1234])
def test_invalid_limits(p: int) -> None:
    assert not is_valid_limit(p)
