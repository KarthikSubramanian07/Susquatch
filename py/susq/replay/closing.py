"""Poll-closing times from `config/poll_closing.toml` (the table is documented in docs/ELECTION_NIGHT.md)."""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_PATH = REPO_ROOT / "config" / "poll_closing.toml"


@dataclass(frozen=True)
class Closing:
    zone: str
    tz: str
    closes_local: str
    closes_utc: datetime


@dataclass(frozen=True)
class StateClosing:
    state: str
    name: str
    verified: bool
    source: str
    closings: tuple[Closing, ...]

    @property
    def last_close_utc(self) -> datetime:
        """When the last polls in the state close (the table orders closings earliest → latest)."""
        return self.closings[-1].closes_utc

    @property
    def first_close_utc(self) -> datetime:
        return self.closings[0].closes_utc


def _parse_utc(s: str) -> datetime:
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=UTC)


def load_closings(path: Path = DEFAULT_PATH) -> dict[str, StateClosing]:
    """Parse the poll-closing table, keyed by USPS code."""
    data = tomllib.loads(path.read_text())
    out: dict[str, StateClosing] = {}
    for st, v in data["states"].items():
        closings = tuple(
            Closing(c["zone"], c["tz"], c["closes_local"], _parse_utc(c["closes_utc"])) for c in v["closings"]
        )
        if not closings:
            raise ValueError(f"{st}: no closings")
        out[st] = StateClosing(st, v["name"], bool(v["verified"]), v["source"], closings)
    return out


def last_close_utc(state: str, path: Path = DEFAULT_PATH) -> datetime:
    return load_closings(path)[state.upper()].last_close_utc
