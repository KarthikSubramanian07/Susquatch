"""Shared fixtures: real 2022 county results (GA Senate by vote method; NH Senate; ME Governor)."""

from __future__ import annotations

from pathlib import Path

import pytest
from susq.replay.closing import StateClosing, load_closings
from susq.replay.history import ContestHistory, load_contest

FIXTURES = Path(__file__).parent / "fixtures"
SAMPLE_CSV = FIXTURES / "county_results_sample.csv"
REPO_ROOT = Path(__file__).resolve().parents[3]
SAMPLE_SPEC = REPO_ROOT / "py" / "susq" / "replay" / "specs" / "sample_2022.toml"


@pytest.fixture(scope="session")
def closings() -> dict[str, StateClosing]:
    return load_closings()


@pytest.fixture(scope="session")
def ga_senate() -> ContestHistory:
    return load_contest(SAMPLE_CSV, year=2022, state="GA", office="US SENATE")


@pytest.fixture(scope="session")
def nh_senate() -> ContestHistory:
    return load_contest(SAMPLE_CSV, year=2022, state="NH", office="US SENATE")
