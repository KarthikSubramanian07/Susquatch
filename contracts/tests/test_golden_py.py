"""Python side of the cross-language golden tests.

1. Committed fixtures must equal what the reference writer produces now (fixtures are not stale).
2. JSON and binary decode to the same message; binary re-encodes byte-for-byte.
3. Unknown future fields are tolerated; target hashes verify.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest
from susq.contracts import (
    bus_pb2,
    decode,
    encode,
    from_json,
    ops_pb2,
    target_content_sha256,
    targets_pb2,
    to_json,
)

FIX = Path(__file__).resolve().parents[1] / "fixtures"
TYPES = {
    "susq.v1.TargetFile": targets_pb2.TargetFile,
    "susq.v1.TargetPointer": targets_pb2.TargetPointer,
    "susq.v1.CountyResultsBatch": bus_pb2.CountyResultsBatch,
    "susq.v1.RacePosteriors": bus_pb2.RacePosteriors,
    "susq.v1.NationalShift": bus_pb2.NationalShift,
    "susq.v1.Heartbeat": bus_pb2.Heartbeat,
    "susq.v1.Kill": bus_pb2.Kill,
    "susq.v1.JournalEntry": ops_pb2.JournalEntry,
    "susq.v1.OrderIntent": ops_pb2.OrderIntent,
    "susq.v1.TraderLease": ops_pb2.TraderLease,
    "susq.v1.StatusSnapshot": ops_pb2.StatusSnapshot,
    "susq.v1.MarketMap": ops_pb2.MarketMap,
}
MANIFEST: dict[str, str] = json.loads((FIX / "manifest.json").read_text())


def _load_writer():  # type: ignore[no-untyped-def]
    spec = importlib.util.spec_from_file_location("make_fixtures", FIX.parent / "tools" / "make_fixtures.py")
    assert spec is not None
    assert spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


@pytest.mark.parametrize("name", [n for n, t in MANIFEST.items() if t in TYPES])
def test_round_trip(name: str) -> None:
    cls = TYPES[MANIFEST[name]]
    raw = (FIX / f"{name}.binpb").read_bytes()
    from_bin = decode(cls, raw)
    from_js = from_json(cls, (FIX / f"{name}.json").read_text())
    assert from_bin == from_js
    assert encode(from_bin) == raw
    assert from_json(cls, to_json(from_bin)) == from_bin


def test_fixtures_are_current() -> None:
    writer = _load_writer()
    for name, msg in writer.cases().items():
        assert (FIX / f"{name}.binpb").read_bytes() == encode(msg), f"{name} stale: rerun make_fixtures.py"


def test_unknown_fields_tolerated() -> None:
    hb = decode(bus_pb2.Heartbeat, (FIX / "heartbeat_unknown_field.binpb").read_bytes())
    hb.DiscardUnknownFields()  # known fields must match; the unknown one is carried, not interpreted
    assert hb == decode(bus_pb2.Heartbeat, (FIX / "heartbeat.binpb").read_bytes())


def test_target_hash() -> None:
    tf = from_json(targets_pb2.TargetFile, (FIX / "target_file.json").read_text())
    assert target_content_sha256(tf) == tf.content_sha256
    tf.positions[0].target_qty += 1
    assert target_content_sha256(tf) != tf.content_sha256
