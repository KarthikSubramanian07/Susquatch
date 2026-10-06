"""Calibration to real money, the chamber audit, and the versioned target-file writer."""

from __future__ import annotations

import json

import pytest
from susq.calibrate.consensus import consensus, left_party, left_target
from susq.calibrate.job import (
    GAP_OK_PTS,
    StaleQuotesError,
    fresh_sim_probs,
    load_sim_probs,
    run_calibration,
    write_outputs,
)
from susq.calibrate.targets import (
    POINTER,
    InputRef,
    TargetValidationError,
    build_target_file,
    read_current,
    validate,
    write_targets,
)
from susq.collectors.quotes import RefQuote
from susq.contracts import from_json, targets_pb2, to_json
from susq.mapping.draft import build_market_map
from susq.models.numpy_engine import NumpySimEngine

from .conftest import NOW_NS


@pytest.fixture(scope="module")
def calib(quotes, cup_markets):
    mm = build_market_map(cup_markets, quotes, seq=1, now_ns=NOW_NS)
    return run_calibration(quotes, NumpySimEngine(), market_map=mm, n_draws=40_000, seed=11, now_ns=NOW_NS)


def test_consensus_removes_overround_and_picks_left_side(quotes) -> None:
    cons = consensus(quotes, now_ns=NOW_NS)
    assert left_party("2026-SEN-NE", cons) == "IND"
    assert left_party("2026-SEN-TX", cons) == "DEM"
    p = left_target("2026-SEN-TX", cons)
    d, r = cons[("2026-SEN-TX", "DEM")].p, cons[("2026-SEN-TX", "REP")].p
    assert p == pytest.approx(d / (d + r))
    assert left_target("2026-SEN-ZZ", cons) is None
    assert consensus(quotes, now_ns=NOW_NS + 10**12, max_age_s=60) == {}


def test_calibrated_marginals_match_targets(calib) -> None:
    for rid, target in calib.targets.items():
        col = calib.draws.column(rid)
        assert (calib.draws.margins[:, col] > 0).mean() == pytest.approx(
            min(max(target, 0.005), 0.995), abs=0.003
        )


def test_race_universe_covers_map_senate_and_house(calib) -> None:
    ids = {s.race_id for s in calib.specs}
    assert "2026-HOUSE-NY-17" in ids
    assert "2026-GOV-GA" in ids
    assert sum(r.startswith("2026-SEN-") for r in ids) == 35  # every seat, priced or not
    assert "2026-CONTROL-HOUSE" in ids
    ne = next(s for s in calib.specs if s.race_id == "2026-SEN-NE")
    assert ne.left_party == "IND"
    assert ne.race_sd > 3.0
    # Races missing from the venue fixture fall back to the incumbent's safe seat.
    assert "2026-SEN-WY" in calib.unpriced
    assert calib.targets["2026-SEN-WY"] == pytest.approx(0.03)


def test_chamber_audit(calib) -> None:
    senate, house = calib.chambers
    assert senate.chamber == "senate"
    assert not senate.calibrated_directly
    assert senate.market is not None
    assert senate.gap_pts == pytest.approx((senate.implied - senate.market) * 100)
    assert house.calibrated_directly
    assert abs(house.gap_pts) < 0.5
    assert calib.sim_probs["2026-CONTROL-SENATE"]["DEM"] == pytest.approx(senate.implied)
    assert calib.sim_probs["2026-SEN-NE"].keys() == {"IND", "REP"}
    # A >4 pt gap would be a warning; the fixture day sits inside the SPEC band.
    assert (abs(senate.gap_pts) > GAP_OK_PTS) == bool(calib.warnings)


def test_big_chamber_gap_is_reported_as_a_bug_warning(quotes) -> None:
    skewed = [
        RefQuote(**{**q.as_row(), "mid": 0.99})
        if q.race_id == "2026-CONTROL-SENATE" and q.party == "DEM"
        else q
        for q in quotes
    ]
    res = run_calibration(skewed, NumpySimEngine(), n_draws=20_000, seed=3, now_ns=NOW_NS)
    assert res.warnings
    assert "assume a bug before assuming edge" in res.warnings[0]


def test_outputs_roundtrip(calib, tmp_path) -> None:
    write_outputs(calib, tmp_path, produced_at_ns=NOW_NS)
    probs, ts = load_sim_probs(tmp_path)
    assert ts == NOW_NS
    assert probs["2026-SEN-NH"]["DEM"] == pytest.approx(calib.sim_probs["2026-SEN-NH"]["DEM"])
    summary = json.loads((tmp_path / "calibration.json").read_text())
    assert summary["engine"] == "numpy"
    assert summary["chambers"][0]["chamber"] == "senate"
    assert load_sim_probs(tmp_path / "nowhere") == ({}, 0)


def _target(**overrides) -> targets_pb2.TargetFile:
    kw = {
        "tournament_slug": "midterm-elections",
        "seq": 7,
        "now_ns": NOW_NS,
        "valid_for_s": 36 * 3600,
        "inputs": [InputRef("reference_mids", "ab" * 32, NOW_NS)],
        "fair": [
            targets_pb2.FairValue(exchange_id="1", market_id="1", p_yes=0.5, p_lo=0.4, p_hi=0.6, source="x")
        ],
        "positions": [
            targets_pb2.TargetPosition(exchange_id="1", market_id="1", target_qty=123, max_entry_price=0.45)
        ],
        "cluster_limits": {"national": 0.25, "house": 0.2},
        "tourney": targets_pb2.TourneyEstimate(p_first=0.02, p_top3=0.05, field_size=2000, scenario="N2"),
        "notes": "test",
    }
    return build_target_file(**{**kw, **overrides})


def test_target_file_written_verified_and_pointed_to(tmp_path) -> None:
    tf = _target()
    path = write_targets(tmp_path, tf, pointer_seq=1)
    assert path.name.startswith("targets-20260929T000000Z-")
    assert path.name.endswith(f"{tf.content_sha256[:8]}.json")
    ptr = from_json(targets_pb2.TargetPointer, (tmp_path / POINTER).read_text())
    assert ptr.file == path.name
    assert ptr.content_sha256 == tf.content_sha256
    back = read_current(tmp_path)
    assert back == tf
    assert [c.cluster for c in back.clusters] == ["house", "national"]
    assert back.valid_until_ns == NOW_NS + 36 * 3600 * 10**9


def test_invalid_target_never_reaches_the_pointer(tmp_path) -> None:
    write_targets(tmp_path, _target(), pointer_seq=1)
    good_ptr = (tmp_path / POINTER).read_text()
    bad = _target(
        seq=8,
        fair=[targets_pb2.FairValue(exchange_id="1", market_id="1", p_yes=0.7, p_lo=0.8, p_hi=0.9)],
    )
    with pytest.raises(TargetValidationError, match="bad fair"):
        write_targets(tmp_path, bad, pointer_seq=2)
    assert (tmp_path / POINTER).read_text() == good_ptr
    assert len(list(tmp_path.glob("targets-*.json"))) == 1


@pytest.mark.parametrize(
    ("mutate", "message"),
    [
        (lambda tf: setattr(tf, "notes", "tampered"), "content_sha256"),
        (lambda tf: tf.header.__setattr__("schema_version", 9), "schema_version"),
        (lambda tf: setattr(tf, "valid_until_ns", 1), "valid_until_ns"),
        (lambda tf: tf.positions[0].__setattr__("max_entry_price", 1.5), "bad position"),
        (lambda tf: tf.tourney.__setattr__("p_first", float("nan")), "tourney"),
    ],
)
def test_validation_catches_corruption(mutate, message) -> None:
    tf = _target()
    mutate(tf)
    with pytest.raises(TargetValidationError, match=message):
        validate(tf)


def test_pointer_hash_mismatch_is_detected(tmp_path) -> None:
    path = write_targets(tmp_path, _target(), pointer_seq=1)
    tf = from_json(targets_pb2.TargetFile, path.read_text())
    tf.notes = "edited after the fact"
    path.write_text(to_json(tf))
    with pytest.raises(TargetValidationError):
        read_current(tmp_path)
    assert read_current(tmp_path / "empty") is None


def test_stale_quotes_refuse_to_calibrate(quotes, cup_markets) -> None:
    """2026-10-06: a dead collector left every race unpriced and the defaults (0.03/0.97) got published."""
    mm = build_market_map(cup_markets, quotes, seq=1, now_ns=NOW_NS)
    with pytest.raises(StaleQuotesError, match="no fresh real-money quote"):
        run_calibration(
            quotes, NumpySimEngine(), market_map=mm, n_draws=2_000, seed=1, now_ns=NOW_NS + 10**9 * 86_400
        )


def test_old_sim_probs_are_not_blended(calib, tmp_path) -> None:
    write_outputs(calib, tmp_path, produced_at_ns=NOW_NS)
    assert fresh_sim_probs(tmp_path, now_ns=NOW_NS + 10**9 * 3600) == calib.sim_probs
    assert fresh_sim_probs(tmp_path, now_ns=NOW_NS + 10**9 * 48 * 3600) == {}
    assert fresh_sim_probs(tmp_path / "missing", now_ns=NOW_NS) == {}
