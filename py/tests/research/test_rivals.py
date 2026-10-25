"""EXP-011 endgame rival model on synthetic leaderboards, price paths and worlds (no real accounts)."""

from __future__ import annotations

import json
import shutil
from datetime import UTC, datetime, timedelta

import numpy as np
import polars as pl
import pytest
from susq.calibrate.pipeline import run_targets
from susq.calibrate.targets import read_current
from susq.contracts import ops_pb2
from susq.models.numpy_engine import NumpySimEngine
from susq.models.types import RaceSpec, SimParams
from susq.optimizer.book import score_scenarios
from susq.optimizer.legs import Leg
from susq.optimizer.scenarios import Scenario
from susq.research_env import OptimizerConfig, Paths, ResearchConfig
from susq.telemetry import rivals
from susq.telemetry.rivals_job import load_book_file, our_book, rival_threshold_for

from .conftest import NOW_NS
from .test_pipeline_scheduler import _prepare_home

T0 = datetime(2026, 10, 20, tzinfo=UTC)
EXCHANGES = [f"e{i}" for i in range(8)]


def _price_paths(hours: int, seed: int = 1) -> np.ndarray:
    """(hours + 1, exchanges) YES prices: bounded random walks, every exchange moving every hour."""
    rng = np.random.default_rng(seed)
    start = rng.uniform(0.1, 0.9, len(EXCHANGES))
    steps = rng.normal(0.0, 0.01, (hours + 1, len(EXCHANGES)))
    steps[0] = 0.0
    return np.clip(start + steps.cumsum(axis=0), 0.02, 0.98)


def _prices_frame(v: np.ndarray, *, step_min: int = 20) -> pl.DataFrame:
    """Per-`step_min` price snapshots whose book mid is the path value at the top of each hour."""
    rows = []
    for h in range(v.shape[0]):
        for m in range(0, 60, step_min):
            ts = T0 + timedelta(hours=h, minutes=m)
            for j, ex in enumerate(EXCHANGES):
                p = float(v[h, j])
                rows.append({"ts": ts, "exchange_id": ex, "market_id": f"m{j}", "latest_price": p,
                             "best_bid": p - 0.005, "best_ask": p + 0.005, "spread": 0.01})  # fmt: skip
    return pl.DataFrame(rows).with_columns(pl.col("ts").cast(pl.Datetime("us", "UTC")))


def _snapshot_ts(hours: int) -> list[datetime]:
    return [T0 + timedelta(hours=h, seconds=30, microseconds=399_710) for h in range(hours + 1)]


def _leaderboard(
    v: np.ndarray,
    books: dict[str, list[tuple[int, dict[int, float]]]],
    *,
    mine: str,
    active_every_hour: frozenset[str] = frozenset(),
    lead_bonus: float = 50_000.0,
    noise: float = 5.0,
    seed: int = 2,
) -> pl.DataFrame:
    """Hourly rows. `books[pid]` = [(from_hour, {exchange index: signed shares}), ...]: the account trades
    at each `from_hour` (its trade count steps), otherwise its P&L moves only by revaluation. The first
    account gets `lead_bonus` of realised P&L so it is the leader."""
    rng = np.random.default_rng(seed)
    first = next(iter(books))
    pnl = {pid: lead_bonus if pid == first else 0.0 for pid in books}
    trades = dict.fromkeys(books, 0)
    rows = []
    for h in range(v.shape[0]):
        for pid, plan in books.items():
            if h == 0:
                continue
            prev = [p for start, p in plan if start <= h - 1][-1]
            pnl[pid] += sum(q * (v[h, j] - v[h - 1, j]) for j, q in prev.items()) + rng.normal(0, noise)
            if any(start == h for start, _ in plan) or pid in active_every_hour:
                trades[pid] += 3
        order = sorted(books, key=lambda pid: -pnl[pid])
        my_rank = order.index(mine) + 1
        ts = T0 + timedelta(hours=h, seconds=30, microseconds=399_710)
        rows += [
            {"ts": ts, "rank": i + 1, "profile_id": pid, "username": f"user-{pid}", "pnl": pnl[pid],
             "trades_count": trades[pid], "volume": 1e5, "win_rate": None, "roi": 0.0, "total": len(books),
             "my_rank": my_rank}
            for i, pid in enumerate(order)
        ]  # fmt: skip
    return pl.DataFrame(rows).with_columns(pl.col("ts").cast(pl.Datetime("us", "UTC")))


HOURS = 120


@pytest.fixture(scope="module")
def synthetic():
    v = _price_paths(HOURS)
    books = {
        "zq-lead": [(0, {0: 40_000.0, 3: -25_000.0})],  # static YES + NO book, marked highest
        "zq-mover": [(0, {1: 30_000.0}), (60, {2: 50_000.0, 5: 20_000.0})],  # rebuilt its book at hour 60
        "zq-bot": [(0, {4: 10_000.0})],
        "zq-self": [(0, {0: 15_000.0})],
    }
    lb = _leaderboard(v, books, mine="zq-self", active_every_hour=frozenset({"zq-bot"}))
    return v, lb


def test_price_panel_folded_per_day_matches_the_in_memory_panel(tmp_path, synthetic) -> None:
    v, _ = synthetic
    prices = _prices_frame(v)
    root = tmp_path / "prices"
    for day, frame in prices.group_by(pl.col("ts").dt.date()):
        (root / f"date={day[0]}").mkdir(parents=True)
        frame.write_parquet(root / f"date={day[0]}" / "p.parquet")
    snaps = _snapshot_ts(HOURS)
    a = rivals.price_panel(prices, snaps)
    b = rivals.read_price_panel(root, snaps)
    assert a.exchanges == b.exchanges == EXCHANGES
    np.testing.assert_allclose(a.values, b.values)
    np.testing.assert_allclose(a.values, v, atol=1e-12)  # mid at the top of the hour, as of +30 s
    assert rivals.price_panel(prices, snaps, source="last").values.shape == v.shape
    with pytest.raises(ValueError, match="valuation source"):
        rivals.price_panel(prices, snaps, source="vwap")


def test_select_rivals_excludes_our_own_row(synthetic) -> None:
    _, lb = synthetic
    ids = rivals.select_rivals(lb, 10)
    assert "zq-self" not in ids
    assert ids[0] == "zq-lead"
    assert rivals.our_rank(lb) is not None


def test_infer_books_recovers_signed_positions_from_quiet_windows(synthetic) -> None:
    v, lb = synthetic
    panel = rivals.price_panel(_prices_frame(v), _snapshot_ts(HOURS))
    books = rivals.infer_books(lb, panel, ["zq-lead", "zq-mover", "zq-bot"], rivals.FitConfig(lookback_h=50))
    lead, mover, bot = books
    assert [b.slot for b in books] == ["R01", "R02", "R03"]
    assert lead.modeled
    assert lead.r2 > 0.95
    assert lead.positions.keys() == {"e0", "e3"}
    assert lead.positions["e0"] == pytest.approx(40_000, rel=0.03)
    assert lead.positions["e3"] == pytest.approx(-25_000, rel=0.03)  # a NO holding comes out negative
    assert lead.hours_since_trade is None
    assert mover.modeled
    assert mover.positions.keys() == {"e2", "e5"}  # the book after its last trade, not the old one
    assert mover.positions["e2"] == pytest.approx(50_000, rel=0.03)
    assert mover.hours_since_trade == pytest.approx(HOURS - 60, abs=0.01)
    assert not bot.modeled
    assert "quiet windows" in bot.reason
    assert lead.balance == pytest.approx(
        100_000 + float(lb.filter(pl.col("profile_id") == "zq-lead")["pnl"][-1])
    )


def test_payout_is_balance_plus_shares_times_outcome_minus_mark() -> None:
    y = np.array([True, False, True, False])
    outcomes = rivals.Outcomes({"a": y}, 4, seed=0)
    # 1,000 YES at 0.2 and 2,000 NO at mark 0.3 (NO marked 0.7)
    m = rivals.payout_multiple(10_000.0, {"a": 1_000.0, "b": -2_000.0}, {"a": 0.2, "b": 0.3}, outcomes)
    b_yes = outcomes.yes("b", 0.3).astype(float)
    expected = (10_000 + 1_000 * (y - 0.2) - 2_000 * (b_yes - 0.3)) / 100_000
    np.testing.assert_allclose(m, np.maximum(expected, 0))
    wiped = rivals.payout_multiple(100.0, {"a": 10_000.0}, {"a": 0.9}, outcomes)
    assert (wiped >= 0).all()
    assert wiped[1] == 0.0  # floored at zero


def test_unsimulated_exchanges_draw_once_and_are_shared() -> None:
    o = rivals.Outcomes({}, 20_000, seed=7)
    a = o.yes("x", 0.25)
    assert a is o.yes("x", 0.25)  # every book holding "x" sees the same worlds
    assert a.mean() == pytest.approx(0.25, abs=0.01)
    assert np.array_equal(a, rivals.Outcomes({}, 20_000, seed=7).yes("x", 0.25))  # deterministic
    assert not o.simulated("x")


def test_outcome_columns_reuse_the_optimizer_payoff_map() -> None:
    specs = [RaceSpec("2026-SEN-AA", "AA", "west", ("house",)), RaceSpec("2026-SEN-BB", "BB", "west")]
    draws = NumpySimEngine().simulate(specs, SimParams(), 5_000, seed=3)
    rules = [
        ops_pb2.MarketRule(
            exchange_id="d", market_id="m", race_id="2026-SEN-AA", party="DEM", kind="race_party"
        ),
        ops_pb2.MarketRule(
            exchange_id="r", market_id="m", race_id="2026-SEN-AA", party="REP", kind="race_party"
        ),
        ops_pb2.MarketRule(exchange_id="n", market_id="m2", race_id="", party=""),
    ]
    cols = rivals.outcome_columns(draws, specs, rules)
    assert set(cols) == {"d", "r"}
    assert np.array_equal(cols["d"], ~cols["r"])


def _worlds(n: int = 20_000, seed: int = 11):
    rng = np.random.default_rng(seed)
    a, b = rng.random(n) < 0.3, rng.random(n) < 0.3
    return rivals.Outcomes({"A": a, "B": b}, n, seed=1), a, b


def _book(slot: str, balance: float, positions: dict[str, float], modeled: bool = True) -> rivals.RivalBook:
    return rivals.RivalBook(
        slot, balance, positions, 0.9, 50, None, modeled, "" if modeled else "fit R2 0.10 < 0.30"
    )


def test_analyze_counts_rivals_on_our_scenario_and_estimates_the_true_threshold() -> None:
    outcomes, a, _ = _worlds()
    marks = {"A": 0.1, "B": 0.1}
    our_pos = {"A": 20_000.0}
    ours = rivals.payout_multiple(100_000, our_pos, marks, outcomes)
    books = [
        _book("R01", 900_000, {"A": 5_000_000.0}),  # marked 9x on A: same scenario as ours
        _book("R02", 300_000, {"B": 1_000_000.0}),  # independent longshot
        _book("R03", 500_000, {}, modeled=False),
    ]
    field_t = np.full(outcomes.n, 2.0)
    r = rivals.analyze(books, marks, outcomes, ours, our_pos, field_t, our_scenario="S-A", scenario_mask=a,
                       titles={"A": "Race A D", "B": "Race B D"}, focus_n=2)  # fmt: skip
    s1, s2, s3 = r.rivals
    assert s1.corr_with_us == pytest.approx(1.0, abs=1e-6)
    assert s1.lift_in_our_scenario > 2.0
    assert abs(s2.corr_with_us) < 0.05
    assert s3.resolved_q == []
    assert not s3.modeled
    assert (r.positively_correlated, r.exposed_to_our_scenario, r.exposed_to_our_scenario_focus) == (1, 1, 1)
    assert s1.exposures[0].title == "Race A D"
    assert s1.exposures[0].ours == "same side"
    assert s1.exposures[0].longshot
    assert s1.longshot_mark_share == pytest.approx(5e5 / 9e5, abs=1e-3)
    # R01 resolves to ~0.5x when A misses (70% of worlds): its 9x mark is mostly a longshot.
    assert s1.p_below_half_mark == pytest.approx(0.7, abs=0.02)
    # 900k mark, 500k visible in A at 0.1: the other 400k is unexplained and is 0 in the low bound
    assert s1.unexplained_share == pytest.approx(4 / 9, abs=1e-3)
    assert s1.resolved_low_q[2] == pytest.approx(s1.resolved_q[2] - 4.0, abs=0.01)
    assert r.true_threshold_low_q[2] <= r.true_threshold_q[2]
    assert r.modeled_wide == 2
    assert r.modeled_focus == 2
    assert r.true_threshold_q[2] >= r.field_threshold_q[2]
    assert r.p_beat_true <= r.p_beat_field
    assert r.cash_floor[0]["place"] == 1
    assert len(r.cash_floor) == 2
    md = rivals.render_markdown(r)
    assert "R01" in md
    assert "Race A D" in md


def test_report_and_book_file_carry_no_identities(tmp_path, synthetic) -> None:
    v, lb = synthetic
    panel = rivals.price_panel(_prices_frame(v), _snapshot_ts(HOURS))
    books = rivals.infer_books(lb, panel, rivals.select_rivals(lb, 3), rivals.FitConfig(lookback_h=50))
    outcomes = rivals.Outcomes({}, 5_000, seed=3)
    marks = panel.latest()
    ours = rivals.payout_multiple(100_000, {}, marks, outcomes)
    rep = rivals.analyze(books, marks, outcomes, ours, {}, np.full(5_000, 3.0))
    text = json.dumps(rep.as_dict()) + rivals.render_markdown(rep)
    book = json.dumps(rivals.RivalBookFile(1, marks, books).as_dict())
    for pid in ("zq-", "user-"):
        assert pid not in text
        assert pid not in book


def test_blend_threshold_is_identity_at_zero_and_rival_only_at_one() -> None:
    t, r = np.array([2.0, 4.0]), np.array([10.0, 1.0])
    assert rivals.blend_threshold(t, r, 0.0) is t
    np.testing.assert_allclose(rivals.blend_threshold(t, r, 1.0), r)
    np.testing.assert_allclose(rivals.blend_threshold(t, r, 0.5), [6.0, 2.5])
    assert rivals.rival_threshold(np.zeros((3, 0))).tolist() == [1.0, 1.0, 1.0]


def test_rival_threshold_moves_the_pick_away_from_the_rivals_scenario() -> None:
    """Two mirror-image scenarios tie on P(M > T); a rival loaded on the first makes the second the pick."""
    n = 4_000
    first = np.zeros(n, dtype=bool)
    first[: n // 2] = True
    outcomes = np.column_stack([first, ~first])
    legs = [
        Leg(
            f"x{i}",
            "m",
            f"leg {i}",
            f"race{i}",
            "DEM",
            0.2,
            None,
            0.5,
            0.6,
            ("national",),
            "",
            "race_party",
            True,
        )
        for i in range(2)
    ]
    scenarios = [Scenario("S1", 9, {"national": 1}, first), Scenario("S2", 0, {"national": -1}, ~first)]
    cfg = OptimizerConfig(min_lift=1.0)
    t = np.full(n, 2.0)
    base = score_scenarios(legs, outcomes, scenarios, t, cfg)
    assert base[0].score == base[1].score  # a tie, broken by id
    book = rivals.RivalBookFile(0, {"x0": 0.2}, [_book("R01", 100_000, {"x0": 1_000_000.0})])
    r, n_rivals = rival_threshold_for(book, {"x0": first, "x1": ~first}, n, seed=0)
    assert n_rivals == 1
    ranked = score_scenarios(legs, outcomes, scenarios, rivals.blend_threshold(t, r, 1.0), cfg)
    assert ranked[0].scenario.scenario_id == "S2"
    assert ranked[0].score > ranked[1].score


def test_load_book_file_refuses_missing_unreadable_and_stale(tmp_path) -> None:
    path = tmp_path / "rivals_book.json"
    assert load_book_file(path, NOW_NS) == (None, "no rival book")
    path.write_text("{")
    assert load_book_file(path, NOW_NS)[1] == "rival book unreadable"
    path.write_text(json.dumps(rivals.RivalBookFile(NOW_NS - 40 * 3600 * 10**9, {}, []).as_dict()))
    assert "old" in load_book_file(path, NOW_NS)[1]
    path.write_text(
        json.dumps(rivals.RivalBookFile(NOW_NS, {"a": 0.1}, [_book("R01", 1.0, {"a": 2.0})]).as_dict())
    )
    book, why = load_book_file(path, NOW_NS)
    assert why == ""
    assert book is not None
    assert book.books[0].positions == {"a": 2.0}


def test_our_book_reads_live_status_then_falls_back(tmp_path) -> None:
    status = tmp_path / "status.json"
    status.write_text(json.dumps({"mode": "live", "equity": 98_000.0, "positions": [
        {"exchangeId": "a", "quantity": 100, "mark": 0.3}, {"exchangeId": "b", "quantity": -50, "mark": 0},
        {"exchangeId": "c", "quantity": 0, "mark": 0.5}]}))  # fmt: skip
    bal, pos, marks, src = our_book(status, tmp_path / "targets", {"b": 0.4}, 100_000.0)
    assert (bal, pos, marks, src) == (98_000.0, {"a": 100.0, "b": -50.0}, {"a": 0.3, "b": 0.4}, "status")
    status.write_text(json.dumps({"mode": "paper", "equity": 1.0}))
    assert our_book(status, tmp_path / "targets", {}, 100_500.0) == (100_500.0, {}, {}, "none")


def test_config_parses_the_knob_and_defaults_off(home) -> None:
    assert home.load_config().optimizer.rival_decorrelation == 0.0  # the repo config ships it off
    cfg = ResearchConfig.from_mapping({"optimizer": {"rival_decorrelation": 0.4}})
    assert cfg.optimizer.rival_decorrelation == 0.4
    assert ResearchConfig.from_mapping({}).optimizer.rival_decorrelation == 0.0


def _write_book(paths: Paths, exchange: str) -> None:
    book = rivals.RivalBookFile(NOW_NS, {exchange: 0.5}, [_book("R01", 400_000, {exchange: 2_000_000.0})])
    paths.research_state.mkdir(parents=True, exist_ok=True)
    (paths.research_state / "rivals_book.json").write_text(json.dumps(book.as_dict()))


def test_targets_are_byte_identical_with_the_knob_at_zero(home, quotes, tmp_path_factory) -> None:
    """With rival_decorrelation = 0.0 and a rival book on disk, the targets file is the same bytes as a run
    with no knob in the config and no book (today's behaviour)."""
    _prepare_home(home, quotes, verify=True)
    today = Paths(tmp_path_factory.mktemp("today") / "home")
    shutil.copytree(home.home, today.home, copy_function=shutil.copy2)  # same inputs, same mtimes
    cfg_text = today.config.read_text()
    stripped = "\n".join(line for line in cfg_text.splitlines() if not line.startswith("rival_decorrelation"))
    today.config.write_text(stripped + "\n")
    assert "rival_decorrelation =" not in today.config.read_text()
    _write_book(home, "1381")

    off = run_targets(home, home.load_config(), quotes, now_ns=NOW_NS, n_draws=10_000, seed=3)
    base = run_targets(today, today.load_config(), quotes, now_ns=NOW_NS, n_draws=10_000, seed=3)
    assert off.path is not None
    assert base.path is not None
    assert off.path.name == base.path.name
    assert off.path.read_bytes() == base.path.read_bytes()
    assert "rival" not in off.notes


def test_targets_use_the_rival_threshold_when_the_knob_is_on(home, quotes) -> None:
    _prepare_home(home, quotes, verify=True)
    cfg = home.load_config()
    on = ResearchConfig.from_mapping({"optimizer": {"rival_decorrelation": 1.0}})
    cfg_on = type(cfg)(**{**cfg.__dict__, "optimizer": type(cfg.optimizer)(**{**cfg.optimizer.__dict__,
                       "rival_decorrelation": on.optimizer.rival_decorrelation})})  # fmt: skip
    missing = run_targets(home, cfg_on, quotes, now_ns=NOW_NS, n_draws=10_000, seed=3, write=False)
    assert "rival decorrelation 1.00 not applied: no rival book" in missing.notes
    _write_book(home, "1381")
    run = run_targets(home, cfg_on, quotes, now_ns=NOW_NS, n_draws=10_000, seed=3)
    assert "rival decorrelation 1.00 over 1 modeled rivals" in run.notes
    tf = read_current(home.targets)
    assert tf is not None
    assert "rival decorrelation 1.00" in tf.notes
