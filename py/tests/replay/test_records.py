from __future__ import annotations

from susq.contracts import bus_pb2
from susq.replay.records import batch_id, batch_to_line, line_to_batch


def _sample() -> bus_pb2.CountyResult:
    r = bus_pb2.CountyResult(
        race_id="2026-SEN-GA",
        state="GA",
        county_fips="13121",
        source="clarity:GA",
        vote_type=bus_pb2.VOTE_TYPE_MAIL,
        total_votes=17,
    )
    r.votes.add(candidate="A", party="DEM", votes=10)
    r.votes.add(candidate="B", party="REP", votes=7)
    return r


def test_batch_id_matches_rust_vector() -> None:
    # Same vector as `batch_id_cross_language_vector` in crates/enr/src/model.rs.
    assert batch_id(_sample()) == "432a72f243d38bf29eb50d61d2a4739d"


def test_batch_id_ignores_order_and_timestamps() -> None:
    a = _sample()
    b = _sample()
    b.observed_at_ns = 5
    b.source_ts_ns = 7
    del b.votes[:]
    b.votes.add(candidate="B", party="REP", votes=7)
    b.votes.add(candidate="A", party="DEM", votes=10)
    assert batch_id(a) == batch_id(b)
    b.votes[0].votes = 8
    assert batch_id(a) != batch_id(b)


def test_line_round_trip() -> None:
    batch = bus_pb2.CountyResultsBatch()
    batch.results.append(_sample())
    batch.header.seq = 3
    batch.header.produced_at_ns = 123
    line = batch_to_line(batch)
    assert "\n" not in line
    assert '"producedAtNs": "123"' in line  # proto3 JSON: int64 as string, camelCase
    assert line_to_batch(line) == batch
