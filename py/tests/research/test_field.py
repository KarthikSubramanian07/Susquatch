"""EXP-004: the tape's cheap-leg footprint, the resolved field maximum, robustness, pipeline wiring."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta

import numpy as np
import polars as pl
import pytest
from susq.calibrate.pipeline import run_targets, select_threshold
from susq.calibrate.targets import read_current
from susq.models.tourney import FieldParams, simulate_threshold
from susq.optimizer.book import MIN_TOP_UNDER, ScenarioScore, robust_choice
from susq.optimizer.scenarios import Scenario
from susq.telemetry import field
from susq.telemetry.field import (
    FieldFootprint,
    SideFootprint,
    build_footprint,
    detect_price_space,
    leaderboard_counts,
    leaderboard_floor,
    price_ladder,
    read_tape_ladder,
    tape_threshold,
)
from susq.telemetry.job import run_telemetry

from .conftest import NOW_NS
from .test_pipeline_scheduler import _prepare_home

T0 = datetime(2026, 10, 1, 12, tzinfo=UTC)
QUIET = FieldParams(n_players=4000, frac_yolo=0.0, frac_bot=0.0)  # passive + consensus only: T <= 1.3


def _tape(
    rows: list[tuple[str, str, float, int]], *, start: datetime = T0, step_h: float = 1.0
) -> pl.DataFrame:
    """(exchange_id, side, price, size) rows as a tape frame, one trade every `step_h` hours."""
    return pl.DataFrame(
        [
            {
                "ts": start + timedelta(hours=i * step_h), "exchange_id": ex, "market_id": f"m{ex}",
                "trade_id": f"t{i}", "side": side, "price": price, "size": size, "volume": None,
                "recorded_at": start + timedelta(hours=i * step_h + 1),
            }
            for i, (ex, side, price, size) in enumerate(rows)
        ]
    ).with_columns(
        pl.col("ts").cast(pl.Datetime("us", "UTC")),
        pl.col("recorded_at").cast(pl.Datetime("us", "UTC")),
        pl.col("volume").cast(pl.Float64),
    )  # fmt: skip


def _leaderboard(volumes: list[float], trades: list[int] | None = None) -> pl.DataFrame:
    n = len(volumes)
    trades = trades or [5] * n
    return pl.DataFrame(
        {
            "ts": [T0] * n, "rank": list(range(1, n + 1)),
            "profile_id": [f"p{i}" for i in range(n)], "username": [f"u{i}" for i in range(n)],
            "pnl": [0.0] * n, "trades_count": trades, "volume": volumes, "win_rate": [None] * n,
            "roi": [0.0] * n, "total": [n] * n, "my_rank": [None] * n,
        }
    ).with_columns(
        pl.col("ts").cast(pl.Datetime("us", "UTC")), pl.col("win_rate").cast(pl.Float64),
        pl.col("my_rank").cast(pl.Int32),
    )  # fmt: skip


def _write_tape(root, tape: pl.DataFrame) -> None:
    for (day,), part in tape.group_by(pl.col("ts").dt.date(), maintain_order=True):
        d = root / f"date={day.isoformat()}"
        d.mkdir(parents=True, exist_ok=True)
        part.write_parquet(d / "part-a.parquet")


def _side(ex: str, side: str, p_bar: float, n: float) -> SideFootprint:
    return SideFootprint(ex, side, n * 100_000.0, n * 100_000.0 / p_bar, p_bar, n, 10)


def _footprint(sides: list[SideFootprint], **kw) -> FieldFootprint:
    base = dict(
        as_of_ns=0, trades=5_000, days=5.0, total_spend=1e7, cheap_spend=sum(s.cheap_spend for s in sides),
        price_space="yes", price_space_votes={"yes": 3, "taker": 0}, stake=100_000.0, n_high_turnover=500,
        n_bot=0, sides=sides, usable=True, reason="ok",
    )  # fmt: skip
    return FieldFootprint(**(base | kw))


# ── footprint aggregation ───────────────────────────────────────────────────────────────────


def test_footprint_aggregates_cheap_spend_per_side_in_yes_space() -> None:
    tape = _tape(
        [
            ("A", "YES", 0.05, 1_000), ("A", "YES", 0.10, 1_000),  # cheap YES: 50 + 100 spent
            ("A", "YES", 0.60, 500),  # expensive YES: not a cheap leg
            ("B", "NO", 0.90, 2_000),  # NO at YES-price 0.90 is NO at 0.10: cheap, 200 spent
            ("B", "NO", 0.30, 100),  # NO at 0.70: expensive
            ("C", "yes", 0.20, 10),  # lower-case side, exactly at the cheap edge
            ("D", None, 0.05, 10), ("D", "YES", 1.0, 10), ("D", "YES", 0.05, 0),  # dropped
        ]
    )  # fmt: skip
    tape = pl.concat([tape, tape.head(1)])  # a repeated pass wrote trade t0 twice
    fp = build_footprint(price_ladder(tape), as_of_ns=7)
    by = {(s.exchange_id, s.side): s for s in fp.sides}
    assert set(by) == {("A", "YES"), ("B", "NO"), ("C", "YES")}
    assert by["A", "YES"].cheap_spend == pytest.approx(150.0)
    assert by["A", "YES"].cheap_shares == pytest.approx(2_000)
    assert by["A", "YES"].p_bar == pytest.approx(0.075)  # spend-weighted: 150 / 2,000 shares
    assert by["A", "YES"].multiple == pytest.approx(1 / 0.075)
    assert by["B", "NO"].cheap_spend == pytest.approx(200.0)
    assert by["B", "NO"].p_bar == pytest.approx(0.10)
    assert by["C", "YES"].p_bar == pytest.approx(0.20)
    assert fp.trades == 6  # three invalid rows dropped, the repeat counted once
    assert fp.total_spend == pytest.approx(150 + 300 + 200 + 70 + 2)
    assert fp.cheap_spend == pytest.approx(352.0)
    assert fp.price_space == "yes"
    assert fp.n_bot is None  # no leaderboard
    assert fp.as_of_ns == 7
    assert not fp.usable
    assert "trades" in fp.reason


def test_price_space_is_detected_from_the_two_sides_vwaps() -> None:
    # YES space: on a 0.85 favourite both takers trade near 0.85 (the NO taker hits the YES bid).
    yes_space = [
        ("E", "YES", 0.86, 100),
        ("E", "NO", 0.84, 100),
        ("F", "YES", 0.10, 100),
        ("F", "NO", 0.09, 100),
    ]
    space, votes = detect_price_space(price_ladder(_tape(yes_space)))
    assert (space, votes) == ("yes", {"yes": 2, "taker": 0})
    # Taker space: the NO taker's price is its own, about 1 - 0.85.
    taker = [("E", "YES", 0.86, 100), ("E", "NO", 0.15, 100), ("F", "YES", 0.10, 100), ("F", "NO", 0.91, 100)]
    space, votes = detect_price_space(price_ladder(_tape(taker)))
    assert (space, votes) == ("taker", {"yes": 0, "taker": 2})
    fp = build_footprint(price_ladder(_tape(taker)))
    by = {(s.exchange_id, s.side): s.p_bar for s in fp.sides}
    assert by == {("E", "NO"): pytest.approx(0.15), ("F", "YES"): pytest.approx(0.10)}
    # A coin-flip market cannot tell the spaces apart and does not vote; no evidence keeps YES space.
    assert (
        detect_price_space(price_ladder(_tape([("G", "YES", 0.5, 100), ("G", "NO", 0.5, 100)])))[0] == "yes"
    )
    assert detect_price_space(price_ladder(pl.DataFrame())) == ("yes", {"yes": 0, "taker": 0})


def test_ladder_is_folded_one_day_at_a_time(tmp_path) -> None:
    rows = [("A", "YES", 0.05, 10), ("B", "NO", 0.95, 20), ("A", "YES", 0.05, 30)] * 20
    tape = _tape(rows, step_h=2.0)  # five days
    root = tmp_path / "tape"
    _write_tape(root, tape)
    (root / "state.json").write_text("{}")  # the recorder's cursor file is not a partition
    assert len(list(root.glob("date=*"))) == 6
    folded = read_tape_ladder(root).sort("exchange_id", "side", "price")
    whole = price_ladder(tape).sort("exchange_id", "side", "price")
    assert folded.select("exchange_id", "side", "price", "shares", "trades").equals(
        whole.select("exchange_id", "side", "price", "shares", "trades")
    )
    assert read_tape_ladder(tmp_path / "missing").is_empty()


# ── all-in counts ───────────────────────────────────────────────────────────────────────────


def test_all_in_counts_from_the_leaderboard_turnover() -> None:
    # 4 accounts turned over >= 100% (volumes 100k..400k: median 250k); two have bot-like trade counts.
    lb = _leaderboard([400_000, 300_000, 200_000, 100_000, 50_000, 0], trades=[900, 250, 3, 3, 3, 3])
    stake, n_hi, n_bot = leaderboard_counts(lb)
    assert (stake, n_hi, n_bot) == (250_000.0, 4, 2)
    assert leaderboard_counts(_leaderboard([50_000])) == (100_000.0, 0, 0)  # floored at one balance
    assert leaderboard_counts(pl.DataFrame()) == (100_000.0, 0, None)

    # 500k spent cheap on A YES is 2 stakes; 2.5M on B NO would be 10, capped at n_hi = 4; then Σ n <= 4.
    tape = _tape([("A", "YES", 0.05, 10_000_000), ("B", "NO", 0.95, 50_000_000)])
    fp = build_footprint(price_ladder(tape), lb)
    by = {(s.exchange_id, s.side): s.n_all_in for s in fp.sides}
    assert by["A", "YES"] == pytest.approx(2.0 * 4 / 6)
    assert by["B", "NO"] == pytest.approx(4.0 * 4 / 6)
    assert sum(by.values()) == pytest.approx(4.0)
    assert (fp.stake, fp.n_high_turnover, fp.n_bot) == (250_000.0, 4, 2)


def test_usability_needs_enough_tape() -> None:
    rows = [(f"X{i % 8}", "YES", 0.04 + 0.01 * (i % 8), 100) for i in range(field.MIN_TRADES)]
    long = build_footprint(price_ladder(_tape(rows, step_h=0.25)))  # 1,000 trades over ~10 days, 8 sides
    assert long.usable
    assert long.reason == "ok"
    short = build_footprint(price_ladder(_tape(rows, step_h=0.01)))  # same trades within 10 hours
    assert not short.usable
    assert "days" in short.reason
    few = build_footprint(price_ladder(_tape([("A", "YES", 0.05, 1)] * field.MIN_TRADES, step_h=0.25)))
    assert not few.usable
    assert "cheap sides" in few.reason
    assert build_footprint(pl.DataFrame()).reason == "no tape"
    assert FieldFootprint.from_dict(json.loads(json.dumps(long.as_dict()))) == long


# ── the resolved field maximum ──────────────────────────────────────────────────────────────


def test_threshold_jumps_to_one_over_p_bar_only_where_the_heavy_cheap_leg_pays() -> None:
    rng = np.random.default_rng(1)
    worlds = 20_000
    pays = rng.random(worlds) < 0.05
    outcomes = np.column_stack([pays, rng.random(worlds) < 0.5])
    fp = _footprint([_side("A", "YES", 0.05, 30.0)])  # 30 accounts all in on A YES at 0.05
    t = tape_threshold(outcomes, ["A", "B"], fp, QUIET, np.random.default_rng(2))
    assert np.allclose(t[pays], 20.0)
    assert t[~pays].max() <= 1.3  # a quiet world sits at the consensus top
    assert t[~pays].min() >= 1.0
    # The NO side of the same contract pays in the other worlds.
    t_no = tape_threshold(
        outcomes, ["A", "B"], _footprint([_side("A", "NO", 0.10, 3.0)]), QUIET, np.random.default_rng(2)
    )
    assert np.allclose(t_no[~pays], 10.0)
    assert t_no[pays].max() <= 1.3
    # The largest multiple that pays wins; a floor lifts quiet worlds only.
    both = _footprint([_side("A", "YES", 0.05, 30.0), _side("B", "YES", 0.125, 30.0)])
    t2 = tape_threshold(outcomes, ["A", "B"], both, QUIET, np.random.default_rng(2), floor=3.0)
    assert np.allclose(t2[pays], 20.0)
    assert np.allclose(t2[~pays & outcomes[:, 1]], 8.0)
    assert np.allclose(t2[~pays & ~outcomes[:, 1]], 3.0)


def test_an_account_marked_at_ten_x_from_a_095_buy_resolves_to_nothing() -> None:
    # The leaderboard's 9.9x mark: a whole balance bought at 0.95 on a ~1% outcome in a thin book.
    tape = _tape([("Z", "YES", 0.95, 105_000)] * 3 + [("A", "YES", 0.05, 1_000)])
    fp = build_footprint(price_ladder(tape), _leaderboard([300_000, 300_000, 300_000]))
    assert {(s.exchange_id, s.side) for s in fp.sides} == {("A", "YES")}  # 0.95 is not a cheap leg
    outcomes = np.zeros((5_000, 2), dtype=bool)  # Z resolves NO (and A too)
    base = tape_threshold(outcomes, ["Z", "A"], _footprint([]), QUIET, np.random.default_rng(3))
    got = tape_threshold(outcomes, ["Z", "A"], fp, QUIET, np.random.default_rng(3))
    assert np.array_equal(got, base)
    assert got.max() <= 1.3
    outcomes[:, 0] = True  # even if the 1% outcome happens, 1/0.95 is below the consensus top
    assert np.array_equal(tape_threshold(outcomes, ["Z", "A"], fp, QUIET, np.random.default_rng(3)), base)


def test_fractional_counts_and_unsimulated_contracts_are_probabilities() -> None:
    worlds = 40_000
    outcomes = np.ones((worlds, 1), dtype=bool)  # A always pays
    t = tape_threshold(
        outcomes, ["A"], _footprint([_side("A", "YES", 0.04, 0.3)]), QUIET, np.random.default_rng(4)
    )
    assert (t == 25.0).mean() == pytest.approx(0.3, abs=0.01)  # held with probability n = 0.3
    # A side whose exchange is not simulated pays independently with probability p̄.
    t = tape_threshold(
        outcomes, ["A"], _footprint([_side("Q", "YES", 0.10, 5.0)]), QUIET, np.random.default_rng(4)
    )
    assert (t == 10.0).mean() == pytest.approx(0.10, abs=0.01)


def test_tape_threshold_keeps_the_consensus_and_bot_types_and_drops_yolo() -> None:
    outcomes = np.zeros((10_000, 1), dtype=bool)
    params = FieldParams(n_players=4000, frac_yolo=0.15, frac_bot=0.01)
    t = tape_threshold(outcomes, ["A"], _footprint([]), params, np.random.default_rng(5))
    ref = simulate_threshold(
        outcomes, np.array([0.5]), FieldParams(n_players=4000, frac_yolo=0.0, frac_bot=0.01), seed=5
    )
    assert np.allclose(t, ref)  # same draws as the SPEC model with its YOLO share removed
    seeded = field.tape_field_params(4000, _footprint([], n_bot=40))
    assert seeded.count(seeded.frac_bot) == 40
    assert seeded.frac_yolo == 0.0
    assert field.tape_field_params(4000, _footprint([], n_bot=None)) == FieldParams(
        n_players=4000, frac_yolo=0.0
    )


def test_leaderboard_floor_is_the_third_mark_capped_at_the_fit() -> None:
    assert leaderboard_floor(None) == 1.0
    assert leaderboard_floor({"top_k_multiples": [9.9, 8.0, 4.2, 3.0], "threshold": {"p50": 15.0}}) == 4.2
    assert leaderboard_floor({"top_k_multiples": [9.9, 8.0, 7.0], "threshold": {"p50": 5.0}}) == 5.0
    assert leaderboard_floor({"top_k_multiples": [0.9], "threshold": None}) == 1.0


# ── robustness ──────────────────────────────────────────────────────────────────────────────


def _ranking(*ids: str) -> list[ScenarioScore]:
    mask = np.ones(10, dtype=bool)
    return [ScenarioScore(Scenario(i, 0, {}, mask), 0.0, (0,), (1,), 0.0, 0.0, 0.0) for i in ids]


def test_robust_choice_rule() -> None:
    assert MIN_TOP_UNDER == 2
    # The fitted winner is also top under "tail": committed as is.
    r = robust_choice({"fitted": _ranking("S1", "S2"), "thin": _ranking("S2"), "medium": _ranking("S3"),
                       "tail": _ranking("S1")})  # fmt: skip
    assert (r.chosen, r.top_under, r.fallback) == ("S1", ("fitted", "tail"), False)
    assert "S1 (the fitted winner) is top under 2/4 [fitted, tail]" in r.describe()
    # The fitted winner is top only under itself; S2 wins thin and medium: fall back to S2.
    r = robust_choice({"fitted": _ranking("S1", "S3", "S2"), "thin": _ranking("S2"), "medium": _ranking("S2"),
                       "tail": _ranking("S3")})  # fmt: skip
    assert (r.chosen, r.top_under, r.fallback) == ("S2", ("thin", "medium"), True)
    # The fallback is the scenario top under two of the other three assumptions.
    r = robust_choice({"fitted": _ranking("S1", "S3", "S2"), "thin": _ranking("S2"), "medium": _ranking("S3"),
                       "tail": _ranking("S3")})  # fmt: skip
    assert r.chosen == "S3"
    # Four different tops: nothing is committed.
    r = robust_choice({"fitted": _ranking("S1"), "thin": _ranking("S2"), "medium": _ranking("S3"),
                       "tail": _ranking("S4")})  # fmt: skip
    assert r.chosen is None
    assert "nothing committed" in r.describe()


# ── pipeline ────────────────────────────────────────────────────────────────────────────────


def _synthetic_tape(exchanges: dict[str, float], *, start: datetime) -> pl.DataFrame:
    """A tape on the fixture's exchanges: lottery buys on every cheap side, consensus trades elsewhere."""
    rows = []
    for _ in range(40):
        for ex, p in exchanges.items():
            if p <= 0.20:
                rows.append((ex, "YES", p, 40_000))  # cheap YES: the YOLO footprint
            elif p >= 0.80:
                rows.append((ex, "NO", p, 40_000))  # NO on a favourite is a cheap NO leg
            else:
                rows.append((ex, "YES", p, 500))
    return _tape(rows, start=start, step_h=0.1)


def _cup_prices(home) -> dict[str, float]:
    df = pl.read_parquet(home.data / "cup" / "prices" / "p.parquet")
    return dict(zip(df["exchange_id"].to_list(), df["latest_price"].to_list(), strict=True))


def test_pipeline_uses_the_tape_threshold_and_records_robustness(home, quotes) -> None:
    _prepare_home(home, quotes, verify=True)
    cup = home.data / "cup"
    start = datetime.fromtimestamp(NOW_NS / 1e9, tz=UTC) - timedelta(days=5)
    _write_tape(cup / "tape", _synthetic_tape(_cup_prices(home), start=start))
    (cup / "leaderboard").mkdir()
    lb = _leaderboard([250_000.0] * 300 + [20_000.0] * 900).with_columns(
        pl.lit(start + timedelta(days=4)).cast(pl.Datetime("us", "UTC")).alias("ts")
    )
    lb.write_parquet(cup / "leaderboard" / "lb.parquet")
    doc = run_telemetry(home, home.load_config(), now_ns=NOW_NS)
    fp = doc["field"]
    assert fp["usable"], fp["reason"]
    assert fp["n_high_turnover"] == 300
    assert fp["stake"] == 250_000.0
    text = (home.research_state / "telemetry.json").read_text()
    assert '"t0"' not in text  # aggregates only
    assert "trade_id" not in text

    run = run_targets(home, home.load_config(), quotes, now_ns=NOW_NS, n_draws=20_000, seed=3)
    assert "T from tape(N=1200" in run.notes, run.notes
    assert "robustness: " in run.notes
    r = run.optimizer.robustness
    assert r is not None
    assert set(r.tops) == {"fitted", "thin", "medium", "tail"}
    tf = read_current(home.targets)
    assert tf is not None
    if r.chosen is None:
        assert not tf.positions
    else:
        assert tf.tourney.scenario == r.chosen
        assert len(r.top_under) >= MIN_TOP_UNDER
    assert tf.tourney.threshold_t_p90 >= tf.tourney.threshold_t_p50 >= 1.0


def test_pipeline_falls_back_without_a_usable_footprint(home, quotes) -> None:
    _prepare_home(home, quotes, verify=True)
    run = run_targets(home, home.load_config(), quotes, now_ns=NOW_NS, n_draws=10_000, seed=3)
    assert "T from field-model(" in run.notes  # no telemetry at all
    assert "robustness: " in run.notes

    outcomes = np.zeros((1_000, 1), dtype=bool)
    thin = _footprint([_side("A", "YES", 0.05, 3.0)], usable=False, reason="12 trades < 1000")
    fit = {"p50": 12.0, "p90": 30.0, "field_size": 4000, "method": "leaderboard+prior(w=0.50)"}
    tele = {"leaderboard": {"field_size": 4000, "threshold": fit}, "field": thin.as_dict()}
    t, how = select_threshold(tele, outcomes, ["A"], np.array([0.05]), seed=1)
    assert how == "leaderboard+prior(w=0.50); tape not used: 12 trades < 1000"
    assert np.median(t) == pytest.approx(12.0, rel=0.1)
    tele["field"] = {"broken": True}
    assert select_threshold(tele, outcomes, ["A"], np.array([0.05]), seed=1)[1] == fit["method"]
    tele["field"] = _footprint([_side("A", "YES", 0.05, 3.0)]).as_dict()
    tele["leaderboard"]["top_k_multiples"] = [9.9, 6.0, 2.0]
    t, how = select_threshold(tele, outcomes, ["A"], np.array([0.05]), seed=1)
    assert how.startswith("tape(N=4000, bots=0, floor=2.00x;")
    assert np.allclose(t, 2.0)  # A never pays here: every world sits on the leaderboard floor
