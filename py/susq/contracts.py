"""Helpers around the generated contract types in `susq.v1` (never hand-edit those).

Everything that crosses a process boundary goes through here so that the encoding rules
(deterministic binary, canonical proto3 JSON, header stamping, target hashing) live in one place
per language. Rust mirrors these in `susq-contracts`, and C++ in `cpp/contracts`.
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import time
from pathlib import Path

from google.protobuf import json_format
from google.protobuf.message import Message

from susq.v1 import bus_pb2, common_pb2, needle_pb2, ops_pb2, targets_pb2

SCHEMA_VERSION: int = common_pb2.SCHEMA_VERSION_CURRENT

__all__ = [
    "SCHEMA_VERSION",
    "bus_pb2",
    "common_pb2",
    "decode",
    "encode",
    "from_json",
    "git_sha",
    "header",
    "idempotency_key",
    "needle_pb2",
    "ops_pb2",
    "target_content_sha256",
    "targets_pb2",
    "to_json",
    "with_content_hash",
]


def git_sha() -> str:
    """Short SHA of the running checkout, or $SUSQ_GIT_SHA inside containers."""
    if sha := os.environ.get("SUSQ_GIT_SHA"):
        return sha
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--short=12", "HEAD"],  # noqa: S607 - fixed argv
            capture_output=True,
            text=True,
            check=True,
            timeout=2,
            cwd=Path(__file__).parent,
        )
        return out.stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return "unknown"


def header(
    producer: str, seq: int, *, now_ns: int | None = None, sha: str | None = None
) -> common_pb2.Header:
    """Build the standard header every message/file carries."""
    return common_pb2.Header(
        schema_version=SCHEMA_VERSION,
        produced_at_ns=time.time_ns() if now_ns is None else now_ns,
        producer=common_pb2.Producer(name=producer, git_sha=git_sha() if sha is None else sha),
        seq=seq,
    )


def encode(msg: Message) -> bytes:
    """Deterministic binary encoding (field-number order, no map reordering surprises)."""
    return msg.SerializeToString(deterministic=True)


def decode[M: Message](cls: type[M], data: bytes) -> M:
    msg = cls()
    msg.ParseFromString(data)
    return msg


def to_json(msg: Message) -> str:
    """Canonical proto3 JSON: lowerCamelCase names, int64 as strings, defaults omitted."""
    return json_format.MessageToJson(msg, indent=2, sort_keys=False) + "\n"


def from_json[M: Message](cls: type[M], text: str, *, ignore_unknown: bool = True) -> M:
    msg = cls()
    json_format.Parse(text, msg, ignore_unknown_fields=ignore_unknown)
    return msg


def target_content_sha256(tf: targets_pb2.TargetFile) -> str:
    """sha256 over the deterministic encoding with `content_sha256` cleared."""
    clone = targets_pb2.TargetFile()
    clone.CopyFrom(tf)
    clone.content_sha256 = ""
    return hashlib.sha256(encode(clone)).hexdigest()


def with_content_hash(tf: targets_pb2.TargetFile) -> targets_pb2.TargetFile:
    tf.content_sha256 = target_content_sha256(tf)
    return tf


def idempotency_key(namespace: str, intent_id: str, canonical_payload: str) -> str:
    """Deterministic order idempotency key shared with Rust (see docs/RUNTIME.md rule 4).

    canonical_payload is
    "{exchange_id}|{side}|{action}|{quantity}|{price_pips}|{expiration_unix_s}|{tournament_id}"
    with side in {yes,no} and action in {buy,sell}.
    """
    import uuid

    return str(uuid.uuid5(uuid.UUID(namespace), f"{intent_id}|{canonical_payload}"))
