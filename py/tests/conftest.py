"""Fixtures over :mod:`susq.mock.testing`: the mock in-process (ASGI) and on a real socket."""

from __future__ import annotations

from collections.abc import AsyncIterator

import pytest
from susq.mock import MockConfig
from susq.mock.testing import T0, LiveMock, MockEnv, open_mock_env, serve_mock


@pytest.fixture
def mock_config() -> MockConfig:
    return MockConfig(seed=7, frozen_now=T0)


@pytest.fixture
async def env(mock_config: MockConfig) -> AsyncIterator[MockEnv]:
    async with open_mock_env(mock_config) as e:
        yield e


@pytest.fixture
async def live_mock() -> AsyncIterator[LiveMock]:
    async with serve_mock() as live:
        yield live
