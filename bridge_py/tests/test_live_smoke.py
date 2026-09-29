"""Read-only smoke test against the REAL platform. Never runs in CI (``-m "not live"``).

Run on Oct 1 once the key is in ``.env``:  ``uv run pytest -m live bridge_py/tests/test_live_smoke.py``
It places no orders: it resolves the tournament, lists markets and reads one bulk price snapshot.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from susq.platform.client import SuperMarketClient
from susq_bridge.config import load_config

REPO = Path(__file__).resolve().parents[2]
pytestmark = pytest.mark.live


async def test_platform_reads_work_with_our_key() -> None:
    config = load_config(REPO)
    if not config.has_api_key:
        pytest.skip("SUSQ_API_KEY not set")
    async with SuperMarketClient(config.platform.base_url, config.api_key) as client:
        tournament = await client.get_tournament(config.platform.tournament_slug)
        markets = [m async for m in client.iter_tournament_markets(config.platform.tournament_slug)]
        assert tournament.id
        assert markets
        ids = [ex.id for m in markets for ex in m.exchanges]
        prices = await client.get_prices(ids, tournament.id)
        assert len(prices.data) + len(prices.missing_ids) == len(ids)
