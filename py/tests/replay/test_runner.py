"""Runner and chaos: malformed payloads are rejected, drops and kills are survived."""

from __future__ import annotations

import itertools

import pytest
from susq.contracts import bus_pb2, encode
from susq.replay.backends import CppNeedle, load_cpp_needle, select_backend
from susq.replay.closing import StateClosing
from susq.replay.history import ContestHistory
from susq.replay.needle_ref import ReferenceNeedle
from susq.replay.prior import RaceInput, build_prior
from susq.replay.runner import (
    GARBAGE,
    ChaosConfig,
    InvalidPayloadError,
    Kind,
    decode_batch,
    deliveries,
    replay_in_process,
)
from susq.replay.timeline import NS, BuildConfig, RaceSpec, build_replay


@pytest.fixture(scope="module")
def ga_replay(ga_senate: ContestHistory, closings: dict[str, StateClosing]):
    batches, meta = build_replay([RaceSpec("R", ga_senate)], closings, BuildConfig(seed=5))
    prior = build_prior(
        [RaceInput("R", ga_senate, prior_margin_mean=0.0, prior_margin_sd=5.0)],
        {"GA": closings["GA"].last_close_utc},
    )
    return batches, meta, prior


def _needle(prior) -> ReferenceNeedle:
    n = ReferenceNeedle()
    n.load_prior(prior)
    return n


def test_decode_batch_rejects_garbage_and_bad_headers() -> None:
    with pytest.raises(InvalidPayloadError):
        decode_batch(GARBAGE)
    with pytest.raises(InvalidPayloadError):
        decode_batch(encode(bus_pb2.CountyResultsBatch()))  # no header
    b = bus_pb2.CountyResultsBatch()
    b.header.schema_version = 99
    with pytest.raises(InvalidPayloadError):
        decode_batch(encode(b))
    b.header.schema_version = 1
    b.results.add(race_id="R", county_fips="13")
    with pytest.raises(InvalidPayloadError):
        decode_batch(encode(b))


def test_clean_replay_delivers_everything_in_order(ga_replay) -> None:
    batches, _, prior = ga_replay
    rec = replay_in_process(batches, _needle(prior))
    assert rec.batches_delivered == len(batches) == len(rec.samples)
    assert rec.seq_gaps == rec.seq_resets == rec.malformed_rejected == 0
    assert all(s.latency_s >= 0 for s in rec.samples)


def test_feed_drop_skips_batches_and_flags_heartbeats(ga_replay) -> None:
    batches, _, prior = ga_replay
    chaos = ChaosConfig(drop_start_s=1200, drop_duration_s=900)
    rec = replay_in_process(batches, _needle(prior), chaos=chaos)
    dropped = sum(1 for b in batches if 1200 <= b.header.produced_at_ns / NS < 2100)
    assert dropped > 0
    assert rec.batches_delivered == len(batches) - dropped
    assert rec.seq_gaps == 0, "the publisher renumbers what it actually sends"
    assert any(not h.healthy_input for h in rec.health if 1200 <= h.t_s < 2100)
    # Cumulative feeds recover: the final estimate is unaffected.
    clean = replay_in_process(batches, _needle(prior))
    assert rec.samples[-1].races["R"].margin_mean == pytest.approx(clean.samples[-1].races["R"].margin_mean)


def test_malformed_payloads_are_rejected_not_fatal(ga_replay) -> None:
    batches, _, prior = ga_replay
    rec = replay_in_process(batches, _needle(prior), chaos=ChaosConfig(malformed_every=5))
    assert rec.malformed_injected == len(batches) // 5
    assert rec.malformed_rejected == rec.malformed_injected
    assert rec.batches_delivered == len(batches)


def test_publisher_kill_restarts_with_snapshot_and_seq_reset(ga_replay) -> None:
    batches, _, prior = ga_replay
    chaos = ChaosConfig(kill_at_s=1800, restart_after_s=600)
    ds = list(deliveries(batches, chaos))
    kinds = [d.kind for d in ds]
    assert kinds.count(Kind.KILL) == 1
    assert kinds.count(Kind.RESTART) == 1
    assert not any(1800 <= d.t_s < 2400 and d.kind in (Kind.BATCH, Kind.HEARTBEAT) for d in ds)
    rec = replay_in_process(batches, _needle(prior), chaos=chaos)
    assert rec.kills == rec.restarts == rec.seq_resets == 1
    clean = replay_in_process(batches, _needle(prior))
    assert rec.samples[-1].races["R"].margin_mean == pytest.approx(clean.samples[-1].races["R"].margin_mean)


def test_heartbeats_every_interval_while_alive(ga_replay) -> None:
    batches, _, _ = ga_replay
    hbs = [d for d in deliveries(batches, ChaosConfig(heartbeat_s=60)) if d.kind is Kind.HEARTBEAT]
    gaps = {round(b.t_s - a.t_s) for a, b in itertools.pairwise(hbs)}
    assert gaps == {60}


def test_cpp_backend_is_skipped_gracefully_when_missing(monkeypatch) -> None:
    import importlib

    real = importlib.import_module

    def fake_import_module(name: str, *args: object) -> object:
        if name == "susq_models":
            raise ImportError("not built")
        return real(name, *args)  # type: ignore[arg-type]

    monkeypatch.setattr("susq.replay.backends.importlib.import_module", fake_import_module)
    backend, why = load_cpp_needle()
    assert backend is None
    assert "not importable" in why
    chosen, note = select_backend("cpp")
    assert isinstance(chosen, ReferenceNeedle)
    assert "skipped" in note


def test_cpp_binding_without_needle_class_is_skipped(monkeypatch) -> None:
    import types

    monkeypatch.setattr("susq.replay.backends.importlib.import_module", lambda _name: types.ModuleType("m"))
    backend, why = load_cpp_needle()
    assert backend is None
    assert "no needle.Needle" in why


def test_cpp_adapter_speaks_serialized_protos(ga_replay) -> None:
    """A stand-in with the documented binding surface round-trips through CppNeedle."""
    batches, _, prior = ga_replay

    class FakeBinding:
        def __init__(self) -> None:
            self.inner = ReferenceNeedle()

        def load_prior(self, data: bytes) -> None:
            from susq.contracts import decode, needle_pb2

            self.inner.load_prior(decode(needle_pb2.NeedlePrior, data))

        def ingest(self, data: bytes) -> bytes:
            from susq.contracts import decode

            return encode(self.inner.ingest(decode(bus_pb2.CountyResultsBatch, data)))

    cpp = CppNeedle(FakeBinding())
    cpp.load_prior(prior)
    rec = replay_in_process(batches[:20], cpp)
    assert len(rec.samples) == 20
    assert rec.backend == "cpp-needle"
