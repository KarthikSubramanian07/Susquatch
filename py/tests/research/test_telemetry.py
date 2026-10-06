"""Leaderboard telemetry (field size, multiples, jumps, fitted T) and crowd-drift inference."""

from __future__ import annotations

import json
import math
from datetime import UTC, datetime, timedelta

import polars as pl
import pytest
from susq.collectors.store import read_snapshots, write_snapshot
from susq.contracts import ops_pb2
from susq.telemetry import crowd, leaderboard
from susq.telemetry.cupdata import latest_cup_mids, read_leaderboard, read_prices, read_tree, with_cup_mid
from susq.telemetry.job import read_reference_hourly, run_telemetry, tournament_members

T0 = datetime(2026, 10, 2, tzinfo=UTC)
LOCK_NS = int(datetime(2026, 11, 4, 17, tzinfo=UTC).timestamp() * 1e9)


def _leaderboard(days: int = 10) -> pl.DataFrame:
    rows = []
    for h in range(days * 4):  # every 6 hours
        ts = T0 + timedelta(hours=6 * h)
        total = 800 + 10 * h
        for rank in range(1, 21):
            pnl = (20 - rank) * 1000.0 * (1 + 0.02 * h)  # top grows ~2%/snapshot
            if rank == 5 and h == 20:
                pnl = 150_000.0  # a YOLO hit: 1.15x -> 2.5x
            rows.append(
                {
                    "ts": ts, "rank": rank, "profile_id": f"p{rank}", "username": f"user{rank}", "pnl": pnl,
                    "trades_count": 3, "volume": 1e4, "win_rate": None, "roi": 1.0, "total": total,
                    "my_rank": 17,
                }
            )  # fmt: skip
    return pl.DataFrame(rows).with_columns(pl.col("ts").cast(pl.Datetime("us", "UTC")))


def test_leaderboard_summary() -> None:
    s = leaderboard.summarize(_leaderboard(), lock_ns=LOCK_NS)
    assert s is not None
    assert s.field_size == 800 + 10 * 39
    assert s.snapshots == 40
    assert s.my_rank == 17
    assert s.days_observed == pytest.approx(9.75)
    assert s.top1_multiple == pytest.approx(1 + 19_000 * (1 + 0.02 * 39) / 100_000)
    assert s.top_k_multiples == sorted(s.top_k_multiples, reverse=True)
    assert len(s.top_k_multiples) == 10
    assert s.growth_per_day > 0
    assert [(j.rank, round(j.multiple_after, 2)) for j in s.jumps] == [(5, 2.5)]
    assert s.threshold is not None
    assert s.threshold.method.startswith("leaderboard+prior")
    assert s.threshold.p90 > s.threshold.p50 >= 1.0
    payload = json.dumps(s.as_dict())
    assert "user1" not in payload
    assert "p1" not in json.loads(payload)["jumps"][0].values()


def test_summary_of_nothing_is_none() -> None:
    assert leaderboard.summarize(pl.DataFrame(), lock_ns=LOCK_NS) is None


def test_field_size_is_the_larger_of_ranked_and_members() -> None:
    ranked = 800 + 10 * 39
    s = leaderboard.summarize(_leaderboard(), lock_ns=LOCK_NS, members=3641)
    assert s is not None
    assert (s.ranked, s.members, s.field_size) == (ranked, 3641, 3641)
    assert s.threshold is not None
    assert s.threshold.field_size == 3641
    # Members below the ranked total (a stale status file) never shrink the field.
    small = leaderboard.summarize(_leaderboard(), lock_ns=LOCK_NS, members=10)
    assert small is not None
    assert (small.ranked, small.members, small.field_size) == (ranked, 10, ranked)
    doc = s.as_dict()
    assert {"field_size", "ranked", "members"} <= set(doc)


def test_threshold_fit_moves_from_prior_to_evidence() -> None:
    prior = leaderboard.fit_threshold(
        2000, top1_now=40.0, growth_per_day=0.0, days_observed=0, days_to_lock=30
    )
    assert prior.method == "prior"
    assert prior.p50 == pytest.approx(15.0)
    full = leaderboard.fit_threshold(
        2000, top1_now=40.0, growth_per_day=0.0, days_observed=20, days_to_lock=30
    )
    assert full.p50 == pytest.approx(40.0)
    grow = leaderboard.fit_threshold(
        2000, top1_now=2.0, growth_per_day=0.05, days_observed=14, days_to_lock=20
    )
    assert grow.p50 == pytest.approx(2.0 * math.exp(0.05 * 20), rel=1e-9)  # w = 1 after 14 days


def _map() -> ops_pb2.MarketMap:
    rows = [
        ("E1", "2026-SEN-TX", "DEM", ["national", "state:TX", "red-state-reach"]),
        ("E2", "2026-SEN-TX", "REP", ["national", "state:TX", "red-state-reach"]),
        ("E3", "2026-SEN-ME", "DEM", ["national", "state:ME", "new-england-incumbency"]),
    ]
    return ops_pb2.MarketMap(
        markets=[
            ops_pb2.MarketRule(exchange_id=e, market_id=e, race_id=r, party=p, clusters=c)
            for e, r, p, c in rows
        ]
    )


def _prices_and_reference(hours: int = 48) -> tuple[pl.DataFrame, pl.DataFrame]:
    prices, ref = [], []
    for h in range(hours):
        ts = T0 + timedelta(hours=h)
        wiggle = 0.004 * ((h % 3) - 1)
        # Field long D in Texas: the Cup prices D 4 pts rich and R 4 pts cheap, persistently.
        for ex, cup in (
            ("E1", 0.64 + wiggle),
            ("E2", 0.36 - wiggle),
            ("E3", 0.60 + (0.01 if h % 2 else -0.01)),
        ):
            prices.append({"ts": ts, "exchange_id": ex, "market_id": ex, "latest_price": cup,
                           "best_bid": cup - 0.005, "best_ask": cup + 0.005, "spread": 0.01})  # fmt: skip
        for rid, party, mid in (
            ("2026-SEN-TX", "DEM", 0.60),
            ("2026-SEN-TX", "REP", 0.40),
            ("2026-SEN-ME", "DEM", 0.60),
        ):
            ref.append({"ts": ts, "race_id": rid, "party": party, "mid": mid, "venue": "kalshi"})
    cast = pl.col("ts").cast(pl.Datetime("us", "UTC"))
    return pl.DataFrame(prices).with_columns(cast), pl.DataFrame(ref).with_columns(cast)


def test_crowd_drift_detects_persistent_d_lean() -> None:
    prices, ref = _prices_and_reference()
    summary = crowd.infer_drift(prices, ref, _map())
    by_ex = {m.exchange_id: m for m in summary.markets}
    assert by_ex["E1"].mean_pts == pytest.approx(4.0, abs=0.1)
    assert by_ex["E1"].persistent
    assert by_ex["E2"].mean_pts == pytest.approx(4.0, abs=0.1)
    assert by_ex["E2"].persistent
    assert abs(by_ex["E3"].mean_pts) < 0.1
    assert not by_ex["E3"].persistent
    assert summary.cluster_drift_pts["red-state-reach"] == pytest.approx(4.0, abs=0.1)
    assert "new-england-incumbency" not in summary.persistent_clusters


def test_crowd_needs_data() -> None:
    assert crowd.infer_drift(pl.DataFrame(), pl.DataFrame(), _map()).markets == []


def test_exposure_sign_and_bounds() -> None:
    drift = {"national": 3.0, "red-state-reach": 2.0}
    d_world = crowd.exposure({"national": 1, "red-state-reach": 1}, drift)
    r_world = crowd.exposure({"national": -1, "red-state-reach": -1}, drift)
    assert d_world > 0 > r_world
    assert d_world == pytest.approx(-r_world)
    assert r_world > -1.0
    assert d_world < 1.0
    assert crowd.exposure({"national": 1}, {}) == 0.0


def test_cup_readers_and_mid(tmp_path) -> None:
    prices, _ = _prices_and_reference(3)
    (tmp_path / "prices" / "d").mkdir(parents=True)
    prices.write_parquet(tmp_path / "prices" / "d" / "a.parquet")
    assert read_prices(tmp_path).height == 9
    mids = latest_cup_mids(tmp_path)
    assert set(mids) == {"E1", "E2", "E3"}
    assert with_cup_mid(pl.DataFrame(schema={"x": pl.Int8})).columns[-1] == "cup_mid"
    assert read_leaderboard(tmp_path).is_empty()


def test_telemetry_job_writes_aggregates(home) -> None:
    cup = home.data / "cup"
    (cup / "leaderboard").mkdir(parents=True)
    _leaderboard(3).write_parquet(cup / "leaderboard" / "lb.parquet")
    doc = run_telemetry(home, home.load_config(), now_ns=int((T0 + timedelta(days=3)).timestamp() * 1e9))
    assert doc["leaderboard"]["field_size"] > 0
    assert doc["leaderboard"]["members"] is None  # no status.json yet: ranked total only
    assert doc["leaderboard"]["field_size"] == doc["leaderboard"]["ranked"]
    assert doc["crowd"]["cluster_drift_pts"] == {}  # no market map yet: neutral
    text = (home.research_state / "telemetry.json").read_text()
    assert "user" not in text
    assert "profile" not in text


def test_telemetry_job_uses_member_count_from_status(home) -> None:
    cup = home.data / "cup"
    (cup / "leaderboard").mkdir(parents=True)
    _leaderboard(3).write_parquet(cup / "leaderboard" / "lb.parquet")
    home.status.parent.mkdir(parents=True, exist_ok=True)
    home.status.write_text(json.dumps({"mode": "paper", "fieldSize": 3641, "equity": 100000.0}))
    now_ns = int((T0 + timedelta(days=3)).timestamp() * 1e9)
    doc = run_telemetry(home, home.load_config(), now_ns=now_ns)
    lb = doc["leaderboard"]
    assert (lb["ranked"], lb["members"], lb["field_size"]) == (800 + 10 * 11, 3641, 3641)
    assert lb["threshold"]["field_size"] == 3641
    assert lb["threshold"]["p50"] > leaderboard.fit_threshold(910, 1.0, 0.0, 0.0, 30).p50
    # A broken or empty status file falls back to the ranked total, silently.
    home.status.write_text("{not json")
    assert run_telemetry(home, home.load_config(), now_ns=now_ns)["leaderboard"]["field_size"] == 910
    home.status.write_text(json.dumps({"fieldSize": 0}))
    assert tournament_members(home.status) is None
    assert tournament_members(home.status.parent / "missing.json") is None


def test_read_tree_skips_partitions_before_since(tmp_path) -> None:
    """Only `date=` partitions on or after the lookback are opened, so the read cost tracks the lookback."""
    root = tmp_path / "prices"
    for day in ("2026-09-20", "2026-10-01", "2026-10-04"):
        part = root / f"date={day}"
        part.mkdir(parents=True)
        pl.DataFrame({"ts": [datetime.fromisoformat(day).replace(tzinfo=UTC)], "x": [day]}).write_parquet(
            part / "part.parquet"
        )
    since = datetime(2026, 10, 1, tzinfo=UTC)
    got = read_tree(root, since=since)
    assert sorted(got["x"].to_list()) == ["2026-10-01", "2026-10-04"]
    assert read_tree(root)["x"].len() == 3
    assert read_tree(root, since=since, columns=["x"]).columns == ["x"]
    assert read_tree(root, since=since, columns=["x", "missing"]).columns == ["x"]


def test_reference_hourly_is_folded_one_day_at_a_time(tmp_path) -> None:
    """Folding per `date=` partition gives the same hourly table as one read of the whole tree.

    The whole-tree read is what the kernel OOM-killed every 22 s on Oct 5-6; per-day folding bounds
    peak memory to one day of snapshots.
    """
    _, ref = _prices_and_reference(60)  # two and a half days of hourly rows
    root = tmp_path / "reference"
    for row in ref.with_columns(pl.col("ts").dt.epoch("ns").alias("ts_ns")).drop("ts").to_dicts():
        write_snapshot(root, [row | {"title": "extra column, never read"}], row["ts_ns"])
    assert len(list(root.glob("date=*"))) == 3
    since = (T0 + timedelta(days=1)).date()
    folded = read_reference_hourly(root, since=since).sort("hour", "race_id", "party")
    whole = crowd.reference_hourly(
        read_snapshots(root, since=since, columns=("ts", "race_id", "party", "mid"))
    )
    assert folded.equals(whole.sort("hour", "race_id", "party"))
    assert folded["hour"].min() >= datetime.combine(since, datetime.min.time(), tzinfo=UTC)
    assert set(folded.columns) == {"hour", "race_id", "party", "ref_mid"}
    assert crowd.infer_drift_hourly(pl.DataFrame(), folded, ops_pb2.MarketMap()).markets == []
    assert read_reference_hourly(tmp_path / "missing", since=since).is_empty()
