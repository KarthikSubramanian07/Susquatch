"""Shared plumbing for the research side: config, runtime paths, atomic files, sequence numbers.

Everything the research jobs need from `config/susq.toml` is parsed once into frozen dataclasses so
each module takes plain values (and tests can build them without a file). Paths are rooted at
`$SUSQ_HOME` (default: the current directory), matching docs/RUNTIME.md.
"""

from __future__ import annotations

import hashlib
import os
import tempfile
import tomllib
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

PRODUCER = "python-scheduler"


@dataclass(frozen=True)
class FairConfig:
    """`[fair]`: how real money and the sim blend into one fair value."""

    kalshi_weight: float = 0.8
    sim_weight: float = 0.2
    stale_after_s: float = 60.0  # consumers: orders built on an older fair file are cancelled
    mapping_error_pts: float = 25.0
    # Producer side: a reference quote older than this is ignored (≈ 2.5 collector cycles, so one
    # slow or failed cycle does not flip fair values to sim-only). Not in susq.toml yet.
    quote_max_age_s: float = 150.0


@dataclass(frozen=True)
class OptimizerConfig:
    """Scenario-book knobs (SPEC §3 Phase 2). Not in susq.toml yet, so defaults live here."""

    book_frac: float = 0.60  # 50-70% of capital in the scenario book
    max_legs: int = 12
    min_leg_prob_in_scenario: float = 0.30  # "consistent with S": pays at least this often in S...
    min_lift: float = 1.10  # ...and more often than it does overall
    max_leg_frac: float = 0.12
    idiosyncratic_frac: float = 0.015
    crowding_lambda: float = 0.5  # T inflation per unit of field exposure to a scenario
    price_slippage_ticks: int = 2


@dataclass(frozen=True)
class ResearchConfig:
    """The subset of `config/susq.toml` the research jobs read."""

    tournament_slug: str = "midterm-elections"
    lock_utc: datetime = datetime(2026, 11, 4, 17, 0, tzinfo=UTC)
    fair: FairConfig = field(default_factory=FairConfig)
    optimizer: OptimizerConfig = field(default_factory=OptimizerConfig)
    cluster_limits: dict[str, float] = field(
        default_factory=lambda: {"national": 0.25, "region": 0.15, "state": 0.10, "house": 0.20}
    )
    leaderboard_interval_s: float = 3600.0
    price_snapshot_interval_s: float = 60.0
    targets_max_age_h: float = 36.0

    @classmethod
    def from_toml(cls, path: Path) -> ResearchConfig:
        """Parse `config/susq.toml`, keeping defaults for anything absent."""
        raw = tomllib.loads(path.read_text())
        return cls.from_mapping(raw)

    @classmethod
    def from_mapping(cls, raw: dict[str, Any]) -> ResearchConfig:
        fair_raw = raw.get("fair", {})
        fair = FairConfig(
            kalshi_weight=float(fair_raw.get("kalshi_weight", 0.8)),
            sim_weight=float(fair_raw.get("sim_weight", 0.2)),
            stale_after_s=float(fair_raw.get("stale_after_s", 60.0)),
            mapping_error_pts=float(fair_raw.get("mapping_error_pts", 25.0)),
        )
        opt_raw = raw.get("optimizer", {})
        optimizer = OptimizerConfig(
            **{k: v for k, v in opt_raw.items() if k in OptimizerConfig.__annotations__}
        )
        lock = raw.get("calendar", {}).get("lock_utc")
        tele = raw.get("telemetry", {})
        return cls(
            tournament_slug=raw.get("platform", {}).get("tournament_slug", "midterm-elections"),
            lock_utc=_parse_utc(lock) if lock else cls.lock_utc,
            fair=fair,
            optimizer=optimizer,
            cluster_limits={k: float(v) for k, v in raw.get("risk", {}).get("clusters", {}).items()}
            or ResearchConfig().cluster_limits,
            leaderboard_interval_s=float(tele.get("leaderboard_interval_s", 3600)),
            price_snapshot_interval_s=float(tele.get("price_snapshot_interval_s", 60)),
            targets_max_age_h=float(raw.get("targets", {}).get("max_age_h", 36)),
        )


@dataclass(frozen=True)
class Paths:
    """Runtime layout from docs/RUNTIME.md, rooted at `$SUSQ_HOME`."""

    home: Path

    @classmethod
    def from_env(cls) -> Paths:
        return cls(Path(os.environ.get("SUSQ_HOME", ".")).resolve())

    @property
    def config(self) -> Path:
        return self.home / "config" / "susq.toml"

    @property
    def data(self) -> Path:
        return self.home / "data"

    @property
    def state(self) -> Path:
        return self.home / "state"

    @property
    def research_state(self) -> Path:
        """Research-private state (sim outputs, flags, telemetry summaries)."""
        return self.home / "state" / "research"

    @property
    def targets(self) -> Path:
        return self.home / "targets"

    @property
    def reports(self) -> Path:
        return self.home / "reports"

    @property
    def journal(self) -> Path:
        return self.home / "journal"

    @property
    def market_map(self) -> Path:
        return self.state / "market_map.json"

    @property
    def market_map_draft(self) -> Path:
        return self.research_state / "market_map.draft.json"

    @property
    def fair_live(self) -> Path:
        return self.state / "fair_live.json"

    @property
    def status(self) -> Path:
        return self.state / "status.json"

    def load_config(self) -> ResearchConfig:
        return ResearchConfig.from_toml(self.config) if self.config.exists() else ResearchConfig()


def _parse_utc(text: str) -> datetime:
    return datetime.fromisoformat(text.replace("Z", "+00:00")).astimezone(UTC)


def atomic_write_bytes(path: Path, data: bytes) -> None:
    """Write via a temp file in the same directory, fsync, then rename (atomic on POSIX)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def atomic_write_text(path: Path, text: str) -> None:
    atomic_write_bytes(path, text.encode())


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


class SeqCounter:
    """Strictly increasing per-(producer, stream) sequence number, persisted across restarts."""

    def __init__(self, path: Path) -> None:
        self._path = path

    def next(self) -> int:
        last = int(self._path.read_text().strip() or 0) if self._path.exists() else 0
        value = last + 1
        atomic_write_text(self._path, f"{value}\n")
        return value
