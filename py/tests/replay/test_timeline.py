from __future__ import annotations

import statistics
from collections import defaultdict

from susq.contracts import SCHEMA_VERSION
from susq.replay.closing import StateClosing
from susq.replay.history import EARLY, EDAY, MAIL, ContestHistory
from susq.replay.records import batch_id, read_replay, write_replay
from susq.replay.timeline import NS, BuildConfig, RaceSpec, build_replay


def _build(contest: ContestHistory, closings: dict[str, StateClosing], **kw: float) -> tuple[list, object]:
    return build_replay([RaceSpec("2022-SEN-GA", contest)], closings, BuildConfig(**kw))  # type: ignore[arg-type]


def test_streams_are_cumulative_and_monotone(
    ga_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    batches, _ = _build(ga_senate, closings)
    last: dict[tuple[str, int], dict[str, int]] = {}
    for b in batches:
        for r in b.results:
            key = (r.county_fips, r.vote_type)
            now = {c.candidate: c.votes for c in r.votes}
            prev = last.get(key, {})
            assert all(now[c] >= prev.get(c, 0) for c in now), key
            assert r.total_votes == sum(now.values())
            assert r.batch_id == batch_id(r)
            last[key] = now


def test_headers_and_timestamps(ga_senate: ContestHistory, closings: dict[str, StateClosing]) -> None:
    batches, meta = _build(ga_senate, closings)
    seqs = [b.header.seq for b in batches]
    assert seqs == list(range(1, len(batches) + 1))
    assert all(b.header.schema_version == SCHEMA_VERSION for b in batches)
    produced = [b.header.produced_at_ns for b in batches]
    assert produced == sorted(produced)
    assert all(p <= meta.horizon_s * NS + 60 * NS for p in produced)
    assert meta.anchor_utc == "2026-11-04T00:00:00Z"


def test_full_horizon_reaches_the_certified_result(
    ga_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    batches, meta = _build(ga_senate, closings, horizon_s=30 * 24 * 3600.0)
    latest: dict[tuple[str, int], tuple[int, int]] = {}
    for b in batches:
        for r in b.results:
            d = sum(c.votes for c in r.votes if c.party == "DEM")
            rep = sum(c.votes for c in r.votes if c.party == "REP")
            latest[(r.county_fips, r.vote_type)] = (d, rep)
    assert sum(v[0] for v in latest.values()) == 1_946_117
    assert sum(v[1] for v in latest.values()) == 1_908_442
    truth = meta.races["2022-SEN-GA"]
    assert (truth.dem, truth.rep) == (1_946_117, 1_908_442)


def test_night_horizon_leaves_late_votes_uncounted(
    ga_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    _, meta = _build(ga_senate, closings)
    t = meta.races["2022-SEN-GA"]
    assert 0 < t.dem - t.night_dem < 0.02 * t.dem  # provisional and a little late mail


def test_mail_and_early_report_before_election_day(
    ga_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    batches, _ = _build(ga_senate, closings)
    first: dict[tuple[str, int], float] = {}
    for b in batches:
        for r in b.results:
            first.setdefault((r.county_fips, r.vote_type), b.header.produced_at_ns / NS)
    by_type = defaultdict(list)
    for (_, vt), t in first.items():
        by_type[vt].append(t)
    assert statistics.median(by_type[MAIL]) < statistics.median(by_type[EDAY])
    assert statistics.median(by_type[EARLY]) < statistics.median(by_type[EDAY])


def test_deterministic_per_seed(ga_senate: ContestHistory, closings: dict[str, StateClosing]) -> None:
    a, _ = _build(ga_senate, closings, seed=11)
    b, _ = _build(ga_senate, closings, seed=11)
    c, _ = _build(ga_senate, closings, seed=12)
    assert [x.SerializeToString() for x in a] == [x.SerializeToString() for x in b]
    assert [x.SerializeToString() for x in a] != [x.SerializeToString() for x in c]


def test_jsonl_round_trip(tmp_path, ga_senate: ContestHistory, closings: dict[str, StateClosing]) -> None:
    batches, meta = _build(ga_senate, closings)
    path = tmp_path / "ga.jsonl"
    assert write_replay(path, batches, meta) == len(batches)
    first_line = path.read_text().splitlines()[0]
    assert first_line.startswith("{")
    assert first_line.endswith("}")
    meta2, batches2 = read_replay(path)
    assert meta2 == meta
    assert [b.SerializeToString() for b in batches2] == [b.SerializeToString() for b in batches]


def test_multi_state_offsets(
    ga_senate: ContestHistory, nh_senate: ContestHistory, closings: dict[str, StateClosing]
) -> None:
    batches, meta = build_replay(
        [RaceSpec("2022-SEN-GA", ga_senate), RaceSpec("2022-SEN-NH", nh_senate)], closings, BuildConfig()
    )
    # GA closes 7pm ET, NH 8pm ET: NH's clock starts an hour into the replay.
    assert meta.races["2022-SEN-NH"].poll_close_offset_s == 3600.0
    first_nh = min(b.header.produced_at_ns for b in batches for r in b.results if r.race_id == "2022-SEN-NH")
    assert first_nh > 3600 * NS
