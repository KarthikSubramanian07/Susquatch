"""Fair value blending, fallbacks, inversion, staleness, bands, flags and the live file format."""

from __future__ import annotations

import json

import pytest
from hypothesis import given
from hypothesis import strategies as st
from susq.collectors.quotes import KALSHI, POLYMARKET, RefQuote
from susq.contracts import from_json, ops_pb2, targets_pb2
from susq.fair.service import FairService, compute_fair, fair_for_rule
from susq.research_env import FairConfig, SeqCounter

NOW = 2_000_000_000 * 10**9
CFG = FairConfig()


def _q(venue: str, ref: str, mid: float, spread: float = 0.01, age_s: float = 1.0) -> RefQuote:
    return RefQuote(
        venue=venue,
        race_id="2026-SEN-TX",
        kind="race_party",
        party="DEM",
        venue_market_id=ref,
        title="t",
        bid=mid - spread / 2,
        ask=mid + spread / 2,
        last=mid,
        mid=mid,
        spread=spread,
        volume=1.0,
        ts_ns=NOW - int(age_s * 1e9),
    )


def _rule(**kw: object) -> ops_pb2.MarketRule:
    base = {
        "market_id": "201",
        "exchange_id": "1201",
        "race_id": "2026-SEN-TX",
        "party": "DEM",
        "kind": "race_party",
    }
    return ops_pb2.MarketRule(**{**base, **kw})


def test_blend_is_weighted_market_plus_sim() -> None:
    rule = _rule(kalshi_ticker="K")
    fv = fair_for_rule(rule, {"K": _q(KALSHI, "K", 0.60)}, {"2026-SEN-TX": {"DEM": 0.50}}, CFG)
    assert fv is not None
    assert fv.p_yes == pytest.approx(0.8 * 0.60 + 0.2 * 0.50)
    assert fv.source == "0.8*market(kalshi)+0.2*sim"
    assert fv.p_lo < fv.p_yes < fv.p_hi


def test_venues_are_spread_weighted() -> None:
    rule = _rule(kalshi_ticker="K", polymarket_token_id="P")
    quotes = {"K": _q(KALSHI, "K", 0.60, spread=0.01), "P": _q(POLYMARKET, "P", 0.64, spread=0.04)}
    fv = fair_for_rule(rule, quotes, {}, CFG)
    assert fv is not None
    assert fv.source == "market(kalshi+polymarket)"
    assert fv.p_yes == pytest.approx((0.60 / 0.01 + 0.64 / 0.04) / (1 / 0.01 + 1 / 0.04))


def test_inverted_reference_is_complemented() -> None:
    rule = _rule(kalshi_ticker="K", kalshi_inverted=True)
    fv = fair_for_rule(rule, {"K": _q(KALSHI, "K", 0.30)}, {}, CFG)
    assert fv is not None
    assert fv.p_yes == pytest.approx(0.70)


def test_sim_only_and_nothing_at_all() -> None:
    rule = _rule(kalshi_ticker="missing")
    fv = fair_for_rule(rule, {}, {"2026-SEN-TX": {"DEM": 0.55}}, CFG)
    assert fv is not None
    assert fv.source == "sim"
    assert fv.p_yes == 0.55
    assert fv.p_hi - fv.p_lo > 0.15  # sim-only fair values carry a wide band
    assert fair_for_rule(rule, {}, {}, CFG) is None
    assert fair_for_rule(_rule(race_id=""), {}, {"": {"DEM": 0.5}}, CFG) is None


def test_stale_quotes_are_ignored() -> None:
    rule = _rule(kalshi_ticker="K")
    stale = _q(KALSHI, "K", 0.6, age_s=CFG.quote_max_age_s + 1)
    fairs, _ = compute_fair([rule], [stale], {}, CFG, now_ns=NOW)
    assert fairs == []
    fairs, _ = compute_fair([rule], [stale], {"2026-SEN-TX": {"DEM": 0.5}}, CFG, now_ns=NOW)
    assert fairs[0].source == "sim"


def test_one_slow_collector_cycle_does_not_flip_fair_to_sim() -> None:
    """Quotes survive well past one 60 s collect cycle (plus its own run time)."""
    assert CFG.quote_max_age_s >= 2 * 60
    rule = _rule(kalshi_ticker="K")
    fairs, _ = compute_fair([rule], [_q(KALSHI, "K", 0.6, age_s=95)], {}, CFG, now_ns=NOW)
    assert fairs[0].source == "market(kalshi)"


def test_newest_quote_per_reference_wins() -> None:
    rule = _rule(kalshi_ticker="K")
    fairs, _ = compute_fair(
        [rule], [_q(KALSHI, "K", 0.4, age_s=5), _q(KALSHI, "K", 0.6, age_s=1)], {}, CFG, now_ns=NOW
    )
    assert fairs[0].p_yes == pytest.approx(0.6)


def test_mapping_error_flag() -> None:
    rule = _rule(kalshi_ticker="K", title="Will the Democratic Party win the Texas Senate?")
    fairs, flags = compute_fair([rule], [_q(KALSHI, "K", 0.6)], {}, CFG, now_ns=NOW, cup_mids={"1201": 0.30})
    assert len(fairs) == 1
    assert len(flags) == 1
    assert flags[0].gap_pts == pytest.approx(-30.0)
    _, none = compute_fair([rule], [_q(KALSHI, "K", 0.6)], {}, CFG, now_ns=NOW, cup_mids={"1201": 0.50})
    assert none == []


@given(
    mid=st.floats(0.01, 0.99),
    spread=st.floats(0.0, 0.2),
    sim=st.one_of(st.none(), st.floats(0.0, 1.0)),
)
def test_band_always_brackets_fair_inside_unit_interval(mid: float, spread: float, sim: float | None) -> None:
    rule = _rule(kalshi_ticker="K")
    sims = {"2026-SEN-TX": {"DEM": sim}} if sim is not None else {}
    fv = fair_for_rule(rule, {"K": _q(KALSHI, "K", mid, spread=spread)}, sims, CFG)
    assert fv is not None
    assert 0.0 <= fv.p_lo <= fv.p_yes <= fv.p_hi <= 1.0


def test_service_writes_only_header_and_fair(tmp_path) -> None:
    out, flags = tmp_path / "state" / "fair_live.json", tmp_path / "flags.json"
    svc = FairService(
        out_path=out,
        flags_path=flags,
        seq=SeqCounter(tmp_path / "seq"),
        cfg=CFG,
        load_rules=lambda: [_rule(kalshi_ticker="K"), _rule(exchange_id="9", race_id="2026-GOV-XX")],
        load_quotes=lambda: [_q(KALSHI, "K", 0.6)],
        load_sim=lambda: {},
        load_cup_mids=lambda: {"1201": 0.1},
        clock_ns=lambda: NOW,
    )
    assert svc.run_once() == (1, 1)
    assert svc.run_once() == (1, 1)
    doc = json.loads(out.read_text())
    assert set(doc) == {"header", "fair"}
    tf = from_json(targets_pb2.TargetFile, out.read_text())
    assert tf.header.seq == 2
    assert tf.header.produced_at_ns == NOW
    assert tf.header.producer.name == "python-scheduler"
    assert json.loads(flags.read_text())["flags"][0]["exchange_id"] == "1201"
    assert not list(out.parent.glob(".*"))  # no temp files left behind
