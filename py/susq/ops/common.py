"""Shared plumbing for the ops helpers: paths, config, atomic writes, and the ACTION NEEDED queue.

ACTION NEEDED items are plain files, one per item, under `ops/state/actions/<key>.txt`. That keeps them
writable from bash (`ops/backup.sh`) and Python alike. It also makes them idempotent: re-raising an item
rewrites the same file, and resolving it deletes the file. The status pusher folds every open item into
the snapshot, so it shows up on the dashboard and in the next daily summary. Nothing here ever pages
anyone.
"""

from __future__ import annotations

import os
import re
import tempfile
import tomllib
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

_KEY_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,63}$")


def utc_now() -> datetime:
    return datetime.now(UTC)


def parse_utc(text: str) -> datetime:
    """Parse an ISO-8601 UTC timestamp such as `2026-11-04T17:00:00Z`."""
    return datetime.fromisoformat(text.replace("Z", "+00:00")).astimezone(UTC)


def susq_home() -> Path:
    """`$SUSQ_HOME`, or the checkout this module lives in (py/susq/ops/common.py -> repo root)."""
    if env := os.environ.get("SUSQ_HOME"):
        return Path(env)
    return Path(__file__).resolve().parents[3]


def load_config(home: Path) -> dict[str, Any]:
    """Load `config/susq.toml`. A missing file yields `{}`, so every caller falls back to defaults."""
    path = home / "config" / "susq.toml"
    if not path.exists():
        return {}
    with path.open("rb") as fh:
        return tomllib.load(fh)


def ops_setting(config: dict[str, Any], key: str, default: Any) -> Any:
    """Read `[ops].<key>` with a default."""
    return config.get("ops", {}).get(key, default)


def atomic_write_text(path: Path, text: str) -> None:
    """Write via a temp file in the same directory, then rename, so readers never see half a file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(text)
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


@dataclass(frozen=True)
class OpsPaths:
    """Every on-disk location the ops helpers touch, rooted at `$SUSQ_HOME` (see docs/RUNTIME.md)."""

    home: Path

    @property
    def ops_state(self) -> Path:
        return self.home / "ops" / "state"

    @property
    def kill_file(self) -> Path:
        return self.home / "ops" / "KILL"

    @property
    def actions_dir(self) -> Path:
        return self.ops_state / "actions"

    @property
    def status_file(self) -> Path:
        return self.home / "state" / "status.json"

    @property
    def lease_file(self) -> Path:
        return self.ops_state / "trader.lease"

    @property
    def backup_record(self) -> Path:
        return self.ops_state / "backup.json"

    @property
    def selfcheck_record(self) -> Path:
        return self.ops_state / "selfcheck.json"

    @property
    def push_record(self) -> Path:
        return self.ops_state / "status_push.json"

    @property
    def poller_record(self) -> Path:
        return self.ops_state / "kill_poller.json"

    @property
    def events_file(self) -> Path:
        """Ops-originated notable events (one line each) waiting to ride along with the next push."""
        return self.ops_state / "events.log"


class ActionItems:
    """The file-per-item ACTION NEEDED queue in `ops/state/actions/`."""

    def __init__(self, directory: Path) -> None:
        self.directory = directory

    def _path(self, key: str) -> Path:
        if not _KEY_RE.match(key):
            raise ValueError(f"invalid action key {key!r}")
        return self.directory / f"{key}.txt"

    def raise_item(self, key: str, text: str) -> None:
        """Open (or refresh) an item. Idempotent."""
        atomic_write_text(self._path(key), text.strip() + "\n")

    def resolve(self, key: str) -> None:
        """Close an item if it is open."""
        self._path(key).unlink(missing_ok=True)

    def resolve_prefix(self, prefix: str, *, keep: set[str] | None = None) -> None:
        """Close every item whose key starts with `prefix`, except those in `keep`."""
        keep = keep or set()
        if not self.directory.exists():
            return
        for path in self.directory.glob(f"{prefix}*.txt"):
            if path.stem not in keep:
                path.unlink(missing_ok=True)

    def items(self) -> list[str]:
        """Open items, sorted by key for a stable order."""
        if not self.directory.exists():
            return []
        out = []
        for path in sorted(self.directory.glob("*.txt")):
            try:
                text = path.read_text(encoding="utf-8").strip()
            except OSError:
                continue
            if text:
                out.append(text)
        return out


def append_event(paths: OpsPaths, text: str, *, now: datetime | None = None) -> None:
    """Record a notable ops event for the next status push (and so the daily summary)."""
    stamp = (now or utc_now()).strftime("%Y-%m-%dT%H:%M:%SZ")
    paths.ops_state.mkdir(parents=True, exist_ok=True)
    with paths.events_file.open("a", encoding="utf-8") as fh:
        fh.write(f"{stamp} {text.strip()}\n")


def drain_events(paths: OpsPaths) -> list[str]:
    """Take (and clear) queued ops events. Rename-then-read so concurrent appends are never lost."""
    src = paths.events_file
    if not src.exists():
        return []
    taken = src.with_name(f"{src.name}.draining")
    os.replace(src, taken)
    lines = [ln.strip() for ln in taken.read_text(encoding="utf-8").splitlines() if ln.strip()]
    taken.unlink(missing_ok=True)
    return lines
