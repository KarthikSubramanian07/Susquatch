"""Shared fixtures for the susq_models binding tests (skipped when the module is not built)."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

pytest.importorskip("susq_models", reason="build it with `uv sync --group models` (see cpp/README.md)")

# The repo runs pytest with --import-mode=importlib, which does not put test directories on
# sys.path; the numpy oracle `reference_sim` lives next to the tests.
sys.path.insert(0, str(Path(__file__).parent))

REPO = Path(__file__).resolve().parents[2]


@pytest.fixture
def fixtures_dir() -> Path:
    """contracts/fixtures (golden messages shared by every language)."""
    return REPO / "contracts" / "fixtures"
