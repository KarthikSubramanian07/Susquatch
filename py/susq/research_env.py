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
    """Scenario-book knobs (SPEC §3 Phase 2); `[optimizer]` in susq.toml overrides the first five,
    `switch_margin` and `rival_decorrelation`."""

    book_frac: float = 0.60  # 50-70% of capital in the scenario book, once the field has shown itself
    # Buy the book gradually at a smaller size first (from Oct 7), full size from Oct 21 16:00Z.
    early_book_frac: float = 0.30
    early_until_utc: datetime = datetime(2026, 10, 21, 16, 0, tzinfo=UTC)
    max_legs: int = 12
    min_leg_prob_in_scenario: float = 0.30  # "consistent with S": pays at least this often in S...
    min_lift: float = 1.10  # ...and more often than it does overall
    max_leg_frac: float = 0.12
    idiosyncratic_frac: float = 0.015
    crowding_lambda: float = 0.5  # T inflation per unit of field exposure to a scenario
    price_slippage_ticks: int = 2
    # EXP-009 hysteresis: a challenger replaces the incumbent scenario only when its score (closeness while
    # every score is 0) beats the incumbent's by more than this fraction.
    switch_margin: float = 0.25
    # EXP-011 (draft, off): score scenarios against T_w = (1 - w) * T + w * R, R the best resolved multiple
    # among the modeled endgame rivals (`susq research rivals --book-out`). 0.0 leaves the targets unchanged.
    rival_decorrelation: float = 0.0

    def book_frac_at(self, now: datetime) -> float:
        """The share of equity the book may spend at `now`: smaller before `early_until_utc`."""
        return self.early_book_frac if now < self.early_until_utc else self.book_frac


CONCENTRATE_MODES = ("off", "report", "commit")
CONCENTRATE_THRESHOLDS = ("targets", "rivals", "max")


@dataclass(frozen=True)
class ConcentrateConfig:
    """`[optimizer.concentrate]` (EXP-012). Off by default: with mode "off" nothing is computed."""

    mode: str = "off"  # off | report (notes only) | commit (replace the scenario book when it wins)
    max_ask: float = 0.10  # legs dearer than this cannot clear a ~13x bar even all-in
    fracs: tuple[float, ...] = (0.9, 1.0)  # share of equity spent on the book; the rest stays cash
    pair_pool: int = 20  # pairs are drawn from this many best singles
    min_gain: float = 0.005  # commit only when P(1st) beats the scenario book's by this much (absolute)
    price_slippage_ticks: int = 2
    # targets: the run's own T; rivals: EXP-011's best modeled rival R (floored at the leaderboard floor);
    # max: the larger of the two per world
    threshold: str = "targets"

    def __post_init__(self) -> None:
        if self.mode not in CONCENTRATE_MODES:
            raise ValueError(f"[optimizer.concentrate] mode must be in {CONCENTRATE_MODES}: {self.mode!r}")
        if self.threshold not in CONCENTRATE_THRESHOLDS:
            raise ValueError(
                f"[optimizer.concentrate] threshold must be in {CONCENTRATE_THRESHOLDS}: {self.threshold!r}"
            )
        if not self.fracs or any(not (0.0 < f <= 1.0) for f in self.fracs):
            raise ValueError("[optimizer.concentrate] fracs must be in (0, 1]")


@dataclass(frozen=True)
class ResearchConfig:
    """The subset of `config/susq.toml` the research jobs read."""

    tournament_slug: str = "midterm-elections"
    lock_utc: datetime = datetime(2026, 11, 4, 17, 0, tzinfo=UTC)
    fair: FairConfig = field(default_factory=FairConfig)
    optimizer: OptimizerConfig = field(default_factory=OptimizerConfig)
    concentrate: ConcentrateConfig = field(default_factory=ConcentrateConfig)  # EXP-012, off by default
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
        opt_default = OptimizerConfig()
        optimizer = OptimizerConfig(
            book_frac=float(opt_raw.get("book_frac", opt_default.book_frac)),
            early_book_frac=float(opt_raw.get("early_book_frac", opt_default.early_book_frac)),
            early_until_utc=_parse_utc(opt_raw["early_until_utc"])
            if "early_until_utc" in opt_raw
            else opt_default.early_until_utc,
            max_legs=int(opt_raw.get("max_legs", opt_default.max_legs)),
            max_leg_frac=float(opt_raw.get("max_leg_frac", opt_default.max_leg_frac)),
            switch_margin=float(opt_raw.get("switch_margin", opt_default.switch_margin)),
            rival_decorrelation=float(opt_raw.get("rival_decorrelation", opt_default.rival_decorrelation)),
        )
        conc_raw = opt_raw.get("concentrate", {})
        cd = ConcentrateConfig()
        concentrate = ConcentrateConfig(
            mode=str(conc_raw.get("mode", cd.mode)),
            max_ask=float(conc_raw.get("max_ask", cd.max_ask)),
            fracs=tuple(float(f) for f in conc_raw.get("fracs", cd.fracs)),
            pair_pool=int(conc_raw.get("pair_pool", cd.pair_pool)),
            min_gain=float(conc_raw.get("min_gain", cd.min_gain)),
            threshold=str(conc_raw.get("threshold", cd.threshold)),
        )
        lock = raw.get("calendar", {}).get("lock_utc")
        tele = raw.get("telemetry", {})
        return cls(
            tournament_slug=raw.get("platform", {}).get("tournament_slug", "midterm-elections"),
            lock_utc=_parse_utc(lock) if lock else cls.lock_utc,
            fair=fair,
            optimizer=optimizer,
            concentrate=concentrate,
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


def _parse_utc(value: str | datetime) -> datetime:
    """An RFC 3339 string (or a TOML datetime literal) as an aware UTC datetime."""
    if isinstance(value, datetime):
        return value.astimezone(UTC) if value.tzinfo else value.replace(tzinfo=UTC)
    return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(UTC)


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
