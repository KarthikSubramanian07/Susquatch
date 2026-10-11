"""The research target file as the executors see it: last good file plus whether exposure may grow.

The same fail-safe rule as rust-live's ``TargetsReader`` (``crates/targets/src/reader.rs``):

* ``current.json`` is followed, hash-checked and validated (``susq.calibrate.targets.read_current``),
  and must name this tournament;
* a file is adopted only while fresh: produced at most 5 minutes in the future, not past
  ``valid_until_ns``, and younger than ``[targets] max_age_h``;
* a missing, invalid, foreign or stale latest file is refused, but the last good file is kept (its
  locked legs stay locked and reductions toward it continue);
* exposure may grow only when the last good file is the latest file and is still fresh.
"""

from __future__ import annotations

from pathlib import Path

from susq.calibrate.targets import read_current
from susq.contracts import targets_pb2

__all__ = ["FUTURE_SKEW_S", "TargetsReader"]

#: How far ``produced_at_ns`` may lie in the future (clock skew between hosts), as in rust-live.
FUTURE_SKEW_S = 300.0
_NS = 1_000_000_000


class TargetsReader:
    def __init__(
        self,
        targets_dir: Path,
        *,
        tournament_slug: str,
        max_age_s: float,
        future_skew_s: float = FUTURE_SKEW_S,
    ) -> None:
        self.targets_dir = targets_dir
        self.tournament_slug = tournament_slug
        self.max_age_ns = round(max_age_s * _NS)
        self.future_skew_ns = round(future_skew_s * _NS)
        #: Last good target file (kept while later files are refused or it goes stale).
        self.good: targets_pb2.TargetFile | None = None
        #: Why the latest ``current.json`` is not in use, if it is not.
        self.rejection: str | None = None
        #: Why the last good file was no longer fresh at the last refresh, if it was not.
        self.stale: str | None = None

    def _fresh(self, tf: targets_pb2.TargetFile, now_ns: int) -> str | None:
        produced = tf.header.produced_at_ns
        if produced - now_ns > self.future_skew_ns:
            return f"produced in the future ({produced} > {now_ns})"
        if tf.valid_until_ns <= now_ns:
            return f"expired at {tf.valid_until_ns}"
        if now_ns - produced > self.max_age_ns:
            return f"older than {self.max_age_ns / _NS / 3600:g} h"
        return None

    def refresh(self, now_ns: int) -> None:
        """Re-read ``current.json`` and re-evaluate freshness at ``now_ns``."""
        reason: str | None
        try:
            tf = read_current(self.targets_dir)
        except Exception as exc:  # half-written, hash mismatch, invalid content
            reason = f"invalid: {exc}"
        else:
            if tf is None:
                reason = "no current.json"
            elif tf.tournament_slug != self.tournament_slug:
                reason = f"tournament {tf.tournament_slug!r} is not {self.tournament_slug!r}"
            else:
                reason = self._fresh(tf, now_ns)
                if reason is None:
                    self.good = tf
        self.rejection = reason
        self.stale = self._fresh(self.good, now_ns) if self.good is not None else None

    @property
    def increase_allowed(self) -> bool:
        return self.good is not None and self.rejection is None and self.stale is None

    def locked(self) -> set[str]:
        """Exchanges whose leg the last good file locks (never sold or reduced)."""
        return {p.exchange_id for p in self.good.positions if p.locked} if self.good is not None else set()
