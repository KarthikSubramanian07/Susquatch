"""EXP-012: the concentrated longshot book, its pipeline hook and config, and the 5 h VWAP valuation panel."""

from __future__ import annotations

import tomllib
from datetime import UTC, datetime, timedelta
from pathlib import Path

import numpy as np
import polars as pl
import pytest
from susq.calibrate.pipeline import apply_concentrate, concentrate_threshold
from susq.contracts import targets_pb2
from susq.optimizer.book import OptimizerResult
from susq.optimizer.concentrate import best_concentrated, concentrated_positions
from susq.optimizer.legs import Leg
from susq.research_env import ConcentrateConfig, Paths, ResearchConfig
from susq.telemetry.rivals import tape_vwap_panel

REPO = Path(__file__).resolve().parents[3]
EQUITY = 100_000.0


def _leg(x: str, ask: float, state: str = "TX", fair: float | None = None) -> Leg:
    f = ask if fair is None else fair
    return Leg(x, f"m{x}", x, f"2026-SEN-{state}", "DEM", ask, None, f, min(0.995, f + 0.02),
               ("national", f"state:{state}"), state, "race_party", True)  # fmt: skip


def _worlds(n: int = 20_000, seed: int = 3) -> np.ndarray:
    return np.random.default_rng(seed).random(n)


def test_all_in_on_the_cheap_leg_that_clears_the_bar():
    u = _worlds()
    outcomes = np.column_stack([u < 0.06, u < 0.30])  # cheap leg pays 6%, dear leg 30%
    legs = [_leg("a", 0.05), _leg("b", 0.20)]
    book = best_concentrated(
        legs, outcomes, np.full(len(u), 12.0), ConcentrateConfig(max_ask=0.25), equity=EQUITY
    )
    assert book is not None
    assert book.legs == (0,)
    assert book.multiple_if_pays == pytest.approx(20.0, rel=0.01)
    assert book.p_first == pytest.approx(0.06, abs=0.01)


def test_a_cash_floor_is_kept_only_when_it_still_clears_the_bar():
    u = _worlds()
    outcomes = np.column_stack([u < 0.05])
    legs = [_leg("a", 0.04)]  # all-in pays 25x; 90% in pays 22.6x
    cfg = ConcentrateConfig(fracs=(0.9,))
    book = best_concentrated(legs, outcomes, np.full(len(u), 12.0), cfg, equity=EQUITY)
    assert book is not None
    assert book.cost == pytest.approx(0.9 * EQUITY, rel=0.001)
    assert book.multiple_if_pays == pytest.approx(0.1 + 0.9 / 0.04, rel=0.01)
    none = best_concentrated(legs, outcomes, np.full(len(u), 30.0), cfg, equity=EQUITY)
    assert none is not None
    assert none.p_first == 0.0


def test_a_pair_wins_when_two_exclusive_legs_each_clear_the_bar_at_half_size():
    u = _worlds()
    outcomes = np.column_stack([u < 0.10, (u >= 0.10) & (u < 0.20)])  # mutually exclusive, 10% each
    legs = [_leg("a", 0.04, "TX"), _leg("b", 0.04, "OH")]
    book = best_concentrated(
        legs, outcomes, np.full(len(u), 12.0), ConcentrateConfig(fracs=(1.0,)), equity=EQUITY
    )
    assert book is not None
    assert set(book.legs) == {0, 1}
    assert book.p_first == pytest.approx(0.20, abs=0.015)  # a single leg would win only 10% of worlds


def test_legs_over_the_ask_cap_are_never_considered():
    u = _worlds()
    legs = [_leg("a", 0.15)]
    assert (
        best_concentrated(legs, np.column_stack([u < 0.5]), np.full(len(u), 2.0), ConcentrateConfig()) is None
    )


def test_the_bar_is_per_world():
    u = _worlds()
    outcomes = np.column_stack([u < 0.10])
    t = np.where(u < 0.05, 30.0, 12.0)  # in half the worlds where the leg pays, the field is higher still
    book = best_concentrated([_leg("a", 0.05)], outcomes, t, ConcentrateConfig(fracs=(1.0,)), equity=EQUITY)
    assert book is not None
    assert book.p_first == pytest.approx(0.05, abs=0.01)


def test_positions_carry_a_tick_valid_price_cap_and_the_early_reporting_rule():
    u = _worlds(2_000)
    legs = [_leg("a", 0.05, "KY", fair=0.03), _leg("b", 0.05, "CA", fair=0.03)]
    book = best_concentrated(legs, np.column_stack([u < 0.1, u < 0.1]), np.full(len(u), 12.0),
                             ConcentrateConfig(fracs=(1.0,)), equity=EQUITY)  # fmt: skip
    assert book is not None
    pos = concentrated_positions(book, legs, ConcentrateConfig())
    for p in pos:
        assert p.target_qty > 0
        assert p.tranche == "T1"
        assert round(p.max_entry_price / 0.005, 6) == round(p.max_entry_price / 0.005)
        assert p.max_entry_price == pytest.approx(
            0.05
        )  # min(ask + 2 ticks, max(fair_hi, ask)): fair_hi < ask
    by = {p.exchange_id: p for p in pos}
    if "a" in by:
        assert by["a"].locked is False  # KY polls close at 6 pm ET: election-night exit or roll
    if "b" in by:
        assert by["b"].locked is True


def _scenario_result() -> OptimizerResult:
    pos = [targets_pb2.TargetPosition(exchange_id="s", market_id="ms", target_qty=10, max_entry_price=0.3)]
    return OptimizerResult(best=None, ranked=[], positions=pos, p_first=0.0, book_cost=3.0, explanation="x")


def test_apply_concentrate_off_report_commit():
    u = _worlds()
    outcomes = np.column_stack([u < 0.06])
    legs = [_leg("a", 0.05)]
    t = np.full(len(u), 12.0)
    base = _scenario_result()
    off, note, scen = apply_concentrate(base, legs, outcomes, t, ConcentrateConfig(), equity=EQUITY, how="T")
    assert off is base
    assert note == ""
    assert scen is None
    rep, note, scen = apply_concentrate(
        base, legs, outcomes, t, ConcentrateConfig(mode="report"), equity=EQUITY, how="T"
    )
    assert rep is base
    assert "EXP-012 (report" in note
    assert "P(1st)=0.06" in note
    assert scen is None
    com, note, scen = apply_concentrate(
        base, legs, outcomes, t, ConcentrateConfig(mode="commit"), equity=EQUITY, how="T"
    )
    assert scen == "concentrated:a"
    assert [p.exchange_id for p in com.positions] == ["a"]
    assert com.p_first == pytest.approx(0.06, abs=0.01)
    assert "committed" in note
    # no gain over the scenario book: keep it
    strong = OptimizerResult(best=None, ranked=[], positions=base.positions, p_first=0.5)
    kept, note, scen = apply_concentrate(
        strong, legs, outcomes, t, ConcentrateConfig(mode="commit"), equity=EQUITY, how="T"
    )
    assert kept is strong
    assert scen is None
    assert "kept the scenario book" in note


def test_concentrate_threshold_falls_back_to_the_targets_t(tmp_path):
    t = np.full(10, 20.0)
    out = np.zeros((10, 1), dtype=bool)
    paths = Paths(tmp_path)
    same, how = concentrate_threshold(paths, ConcentrateConfig(), {}, t, out, ["a"], seed=1, now_ns=0)
    assert same is t
    assert how == "targets T"
    cfg = ConcentrateConfig(threshold="rivals")
    same, how = concentrate_threshold(paths, cfg, {}, t, out, ["a"], seed=1, now_ns=0)
    assert same is t
    assert "no rival book" in how


def test_config_defaults_off_and_parses():
    assert ResearchConfig().concentrate.mode == "off"
    raw = {
        "optimizer": {"concentrate": {"mode": "report", "threshold": "max", "fracs": [1.0], "max_ask": 0.06}}
    }
    cc = ResearchConfig.from_mapping(raw).concentrate
    assert (cc.mode, cc.threshold, cc.fracs, cc.max_ask) == ("report", "max", (1.0,), 0.06)
    with pytest.raises(ValueError, match="mode"):
        ConcentrateConfig(mode="yolo")
    with pytest.raises(ValueError, match="threshold"):
        ConcentrateConfig(threshold="tape")
    with pytest.raises(ValueError, match="fracs"):
        ConcentrateConfig(fracs=(1.5,))


def test_repo_config_never_ships_commit():
    """Report only: committing the concentrated book is a decision made by hand, never a default."""
    raw = tomllib.loads((REPO / "config" / "susq.toml").read_text())
    cfg = ResearchConfig.from_mapping(raw)
    assert cfg.concentrate.mode in ("off", "report")
    assert cfg.concentrate.threshold == "targets"


def test_tape_vwap_panel_is_the_trailing_5h_vwap():
    t0 = datetime(2026, 10, 20, 12, tzinfo=UTC)
    tape = pl.DataFrame(
        {
            "ts": [t0, t0 + timedelta(hours=1), t0 + timedelta(hours=2), t0 + timedelta(hours=1)],
            "exchange_id": ["1", "1", "1", "2"],
            "price": [0.10, 0.20, 0.40, 0.70],
            "size": [100, 100, 200, 5],
        }
    ).with_columns(pl.col("ts").dt.cast_time_unit("us"))
    snaps = [t0 - timedelta(hours=1), t0 + timedelta(hours=3), t0 + timedelta(hours=6, minutes=30)]
    panel = tape_vwap_panel(tape, snaps)
    assert panel.exchanges == ["1", "2"]
    v = panel.values
    assert np.isnan(v[0, 0])
    assert np.isnan(v[0, 1])
    assert v[1, 0] == pytest.approx((10 + 20 + 80) / 400)
    assert v[1, 1] == pytest.approx(0.70)
    # 6.5 h later only the 0.40 trade is in the window
    assert v[2, 0] == pytest.approx(0.40)
    # 5.5 h after exchange 2's only trade the window is empty: last trade price
    assert v[2, 1] == pytest.approx(0.70)
    assert panel.latest() == {"1": pytest.approx(0.40), "2": pytest.approx(0.70)}
