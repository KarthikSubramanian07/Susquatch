"""Write versioned target files for rust-live: `targets/targets-<utc>-<hash8>.json` + `current.json`.

Order of operations (a reader must never see a pointer to a bad file):

1. build the `TargetFile`, stamp `content_sha256` via `susq.contracts.with_content_hash`;
2. write it atomically under its content-addressed name;
3. read it back, re-parse, re-hash and validate (schema version, probabilities in [0,1] and
   ordered, finite numbers, non-empty header) — any failure deletes the file and raises;
4. only then atomically swap `current.json` (`TargetPointer`).
"""

from __future__ import annotations

import math
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from susq.contracts import (
    SCHEMA_VERSION,
    from_json,
    header,
    target_content_sha256,
    targets_pb2,
    to_json,
    with_content_hash,
)
from susq.research_env import PRODUCER, atomic_write_text

POINTER = "current.json"


class TargetValidationError(ValueError):
    """A target file failed validation; it is never pointed to."""


@dataclass(frozen=True)
class InputRef:
    name: str
    sha256: str
    as_of_ns: int


def build_target_file(
    *,
    tournament_slug: str,
    seq: int,
    now_ns: int,
    valid_for_s: float,
    inputs: Sequence[InputRef],
    fair: Sequence[targets_pb2.FairValue],
    positions: Sequence[targets_pb2.TargetPosition],
    cluster_limits: dict[str, float],
    tourney: targets_pb2.TourneyEstimate,
    notes: str,
) -> targets_pb2.TargetFile:
    tf = targets_pb2.TargetFile(
        header=header(PRODUCER, seq, now_ns=now_ns),
        tournament_slug=tournament_slug,
        valid_until_ns=now_ns + int(valid_for_s * 1e9),
        tourney=tourney,
        notes=notes,
    )
    tf.inputs.extend(targets_pb2.InputHash(name=i.name, sha256=i.sha256, as_of_ns=i.as_of_ns) for i in inputs)
    tf.fair.extend(fair)
    tf.positions.extend(positions)
    tf.clusters.extend(
        targets_pb2.ClusterLimit(cluster=c, max_gross_frac=v) for c, v in sorted(cluster_limits.items())
    )
    return with_content_hash(tf)


def validate(tf: targets_pb2.TargetFile) -> None:
    """Raise TargetValidationError on anything rust-live would (or should) reject."""
    problems: list[str] = []
    if tf.header.schema_version != SCHEMA_VERSION:
        problems.append(f"schema_version {tf.header.schema_version} != {SCHEMA_VERSION}")
    if not tf.header.producer.name or tf.header.produced_at_ns <= 0:
        problems.append("header missing producer or produced_at_ns")
    if tf.valid_until_ns <= tf.header.produced_at_ns:
        problems.append("valid_until_ns not after produced_at_ns")
    if tf.content_sha256 != target_content_sha256(tf):
        problems.append("content_sha256 mismatch")
    for f in tf.fair:
        vals = (f.p_yes, f.p_lo, f.p_hi)
        if not all(math.isfinite(v) and 0.0 <= v <= 1.0 for v in vals) or not f.p_lo <= f.p_yes <= f.p_hi:
            problems.append(f"bad fair for exchange {f.exchange_id}: {vals}")
    for p in tf.positions:
        if not (math.isfinite(p.max_entry_price) and 0.0 < p.max_entry_price < 1.0) or p.target_qty == 0:
            problems.append(f"bad position for exchange {p.exchange_id}")
    for c in tf.clusters:
        if not (math.isfinite(c.max_gross_frac) and 0.0 <= c.max_gross_frac <= 1.0):
            problems.append(f"bad cluster limit {c.cluster}")
    t = tf.tourney
    if not all(math.isfinite(v) and 0.0 <= v <= 1.0 for v in (t.p_first, t.p_top3)):
        problems.append("tourney probabilities out of range")
    if problems:
        raise TargetValidationError("; ".join(problems))


def file_name(tf: targets_pb2.TargetFile) -> str:
    stamp = datetime.fromtimestamp(tf.header.produced_at_ns / 1e9, tz=UTC)
    return f"targets-{stamp:%Y%m%dT%H%M%SZ}-{tf.content_sha256[:8]}.json"


def write_targets(targets_dir: Path, tf: targets_pb2.TargetFile, *, pointer_seq: int) -> Path:
    """Write, verify by reading back, then swap the pointer. Returns the target file path."""
    validate(tf)
    path = targets_dir / file_name(tf)
    atomic_write_text(path, to_json(tf))
    try:
        back = from_json(targets_pb2.TargetFile, path.read_text(), ignore_unknown=False)
        validate(back)
        if back.content_sha256 != tf.content_sha256:
            raise TargetValidationError("read-back hash differs from the written hash")
    except Exception:
        path.unlink(missing_ok=True)
        raise
    pointer = targets_pb2.TargetPointer(
        header=header(PRODUCER, pointer_seq, now_ns=tf.header.produced_at_ns),
        file=path.name,
        content_sha256=tf.content_sha256,
    )
    atomic_write_text(targets_dir / POINTER, to_json(pointer))
    return path


def read_current(targets_dir: Path) -> targets_pb2.TargetFile | None:
    """Follow `current.json` and return the validated target file (None if there is none)."""
    ptr_path = targets_dir / POINTER
    if not ptr_path.exists():
        return None
    ptr = from_json(targets_pb2.TargetPointer, ptr_path.read_text())
    tf = from_json(targets_pb2.TargetFile, (targets_dir / ptr.file).read_text())
    if tf.content_sha256 != ptr.content_sha256:
        raise TargetValidationError("pointer hash does not match the target file")
    validate(tf)
    return tf
