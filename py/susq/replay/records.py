"""CountyResult helpers and the replay file format.

A replay is two files:

* `<name>.jsonl`: one `CountyResultsBatch` per line, compact proto3 JSON. All timestamps
  (`header.produced_at_ns`, `observed_at_ns`, `source_ts_ns`) are **nanoseconds relative to the
  replay anchor** (the earliest poll close among the included states), so a replay can be
  re-anchored to any wall clock.
* `<name>.meta.json`: anchor (UTC), per-race poll close offsets, the truth (final and end-of-night
  results per race) and provenance (inputs, seed).
"""

from __future__ import annotations

import hashlib
import json
import struct
from collections.abc import Iterable, Iterator
from dataclasses import asdict, dataclass, field
from pathlib import Path

from google.protobuf import json_format

from susq.contracts import bus_pb2


def batch_id(r: bus_pb2.CountyResult) -> str:
    """Same recipe as `susq_enr::model::batch_id` (Rust), so ids agree across languages.

    sha256 over: source, race_id, county_fips (each followed by 0x1f); vote_type (i32 LE);
    total_votes (u64 LE); precincts reporting/total (u32 LE); then for each candidate sorted by
    (candidate, party): candidate 0x1f party 0x1f votes (u64 LE). First 32 hex chars.
    """
    h = hashlib.sha256()
    for part in (r.source, r.race_id, r.county_fips):
        h.update(part.encode())
        h.update(b"\x1f")
    h.update(struct.pack("<i", r.vote_type))
    h.update(struct.pack("<Q", r.total_votes))
    h.update(struct.pack("<I", r.precincts_reporting))
    h.update(struct.pack("<I", r.precincts_total))
    for c in sorted(r.votes, key=lambda c: (c.candidate, c.party)):
        h.update(c.candidate.encode())
        h.update(b"\x1f")
        h.update(c.party.encode())
        h.update(b"\x1f")
        h.update(struct.pack("<Q", c.votes))
    return h.hexdigest()[:32]


def batch_to_line(batch: bus_pb2.CountyResultsBatch) -> str:
    """Compact one-line proto3 JSON."""
    return json_format.MessageToJson(batch, indent=None)


def line_to_batch(line: str) -> bus_pb2.CountyResultsBatch:
    msg = bus_pb2.CountyResultsBatch()
    json_format.Parse(line, msg, ignore_unknown_fields=True)
    return msg


@dataclass
class RaceTruth:
    race_id: str
    state: str
    dem: int
    rep: int
    other: int
    night_dem: int
    night_rep: int
    poll_close_offset_s: float

    @property
    def margin_pts(self) -> float:
        tp = self.dem + self.rep
        return 100.0 * (self.dem - self.rep) / tp if tp else 0.0

    @property
    def winner(self) -> str:
        return "DEM" if self.dem > self.rep else "REP"


@dataclass
class ReplayMeta:
    anchor_utc: str
    seed: int
    horizon_s: float
    races: dict[str, RaceTruth] = field(default_factory=dict)
    inputs: list[str] = field(default_factory=list)

    def to_json(self) -> str:
        d = asdict(self)
        return json.dumps(d, indent=2, sort_keys=True) + "\n"

    @classmethod
    def from_json(cls, text: str) -> ReplayMeta:
        d = json.loads(text)
        races = {k: RaceTruth(**v) for k, v in d.pop("races").items()}
        return cls(races=races, **d)


def meta_path(jsonl: Path) -> Path:
    return jsonl.with_name(jsonl.name.removesuffix(".jsonl") + ".meta.json")


def write_replay(path: Path, batches: Iterable[bus_pb2.CountyResultsBatch], meta: ReplayMeta) -> int:
    """Write the JSONL and its meta sidecar; returns the number of batches."""
    path.parent.mkdir(parents=True, exist_ok=True)
    n = 0
    with path.open("w") as f:
        for b in batches:
            f.write(batch_to_line(b) + "\n")
            n += 1
    meta_path(path).write_text(meta.to_json())
    return n


def read_replay(path: Path) -> tuple[ReplayMeta, list[bus_pb2.CountyResultsBatch]]:
    meta = ReplayMeta.from_json(meta_path(path).read_text())
    return meta, list(iter_batches(path))


def iter_batches(path: Path) -> Iterator[bus_pb2.CountyResultsBatch]:
    with path.open() as f:
        for line in f:
            if line.strip():
                yield line_to_batch(line)
