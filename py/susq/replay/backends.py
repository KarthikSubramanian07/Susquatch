"""Needle backends for replays: the C++ needle through `susq_models` when it is built, else the
Python reference estimator.

Expected binding contract for the C++ owner (serialized protobuf at the boundary, so the pybind11
surface stays tiny and versioned by the contracts):

    import susq_models
    n = susq_models.needle.Needle()
    n.load_prior(needle_prior_bytes: bytes) -> None       # susq.v1.NeedlePrior
    n.ingest(batch_bytes: bytes) -> bytes   # susq.v1.CountyResultsBatch in, susq.v1.RacePosteriors out

If the module, the submodule or the class is missing, the C++ path is skipped (never an error).
"""

from __future__ import annotations

import importlib
from typing import Any, Protocol

from susq.contracts import bus_pb2, decode, encode, needle_pb2
from susq.replay.needle_ref import ReferenceNeedle


class NeedleBackend(Protocol):
    name: str

    def load_prior(self, prior: needle_pb2.NeedlePrior) -> None: ...

    def ingest(self, batch: bus_pb2.CountyResultsBatch) -> bus_pb2.RacePosteriors: ...


class CppNeedle:
    """Adapter from the pybind11 `susq_models.needle.Needle` to `NeedleBackend`."""

    name = "cpp-needle"

    def __init__(self, impl: Any) -> None:
        self._impl = impl

    def load_prior(self, prior: needle_pb2.NeedlePrior) -> None:
        self._impl.load_prior(encode(prior))

    def ingest(self, batch: bus_pb2.CountyResultsBatch) -> bus_pb2.RacePosteriors:
        return decode(bus_pb2.RacePosteriors, bytes(self._impl.ingest(encode(batch))))


def load_cpp_needle() -> tuple[CppNeedle | None, str]:
    """Try to construct the C++ needle. Returns (backend or None, human-readable reason)."""
    try:
        module = importlib.import_module("susq_models")
    except ImportError as e:
        return None, f"susq_models not importable ({e})"
    cls = getattr(getattr(module, "needle", None), "Needle", None)
    if cls is None:
        return None, "susq_models has no needle.Needle binding"
    try:
        return CppNeedle(cls()), "susq_models.needle.Needle"
    except Exception as e:  # a binding that fails to construct is a skip, not a crash
        return None, f"susq_models.needle.Needle() failed: {e}"


def select_backend(prefer: str = "cpp") -> tuple[NeedleBackend, str]:
    """`prefer` is "cpp" (fall back to reference) or "reference"."""
    if prefer == "cpp":
        cpp, why = load_cpp_needle()
        if cpp is not None:
            return cpp, why
        return ReferenceNeedle(), f"reference (C++ skipped: {why})"
    if prefer == "reference":
        return ReferenceNeedle(), "reference (requested)"
    raise ValueError(f"unknown backend {prefer!r}")
