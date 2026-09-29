"""Fixtures shared by the ops helper tests: a throwaway $SUSQ_HOME and a controllable clock."""

from __future__ import annotations

import shutil
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from susq.ops.common import OpsPaths

REPO = Path(__file__).resolve().parents[3]
FIXTURES = REPO / "contracts" / "fixtures"
T0 = datetime(2026, 10, 5, 12, 0, 0, tzinfo=UTC)


class Clock:
    """A clock the test moves by hand."""

    def __init__(self, now: datetime = T0) -> None:
        self.now = now

    def __call__(self) -> datetime:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += timedelta(seconds=seconds)


@pytest.fixture
def home(tmp_path: Path) -> Path:
    (tmp_path / "config").mkdir()
    shutil.copy(REPO / "config" / "susq.toml", tmp_path / "config" / "susq.toml")
    for d in ("state", "ops/state", "journal", "data", "reports", "targets"):
        (tmp_path / d).mkdir(parents=True, exist_ok=True)
    return tmp_path


@pytest.fixture
def paths(home: Path) -> OpsPaths:
    return OpsPaths(home)


@pytest.fixture
def clock() -> Clock:
    return Clock()
