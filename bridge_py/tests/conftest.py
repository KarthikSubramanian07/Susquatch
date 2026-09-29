"""Bridge test harness: a full :class:`~susq_bridge.runtime.Bridge` wired to the in-process mock."""

from __future__ import annotations

import asyncio
import shutil
from collections.abc import AsyncIterator
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

import httpx
import pytest
from susq.contracts import common_pb2, header, ops_pb2, targets_pb2, to_json
from susq.mock import MockConfig
from susq.mock.testing import KEY, T0, MockEnv, open_mock_env
from susq.platform.prices import to_pips
from susq_bridge.config import BridgeConfig, load_config
from susq_bridge.runtime import Bridge

REPO = Path(__file__).resolve().parents[2]

# Mock market layout (see py/susq/mock/README.md): market m has exchange m + 1000.
STATES = ["OH", "IA", "ME", "TX", "MI", "NH", "GA", "NE"]


def exchange_state(ex: str) -> str | None:
    idx = (int(ex) - 1301) // 2
    return STATES[idx] if 0 <= idx < len(STATES) else None


@dataclass
class Harness:
    mock: MockEnv
    home: Path
    config: BridgeConfig
    sleeps: list[float]

    @property
    def mx(self):  # type: ignore[no-untyped-def]
        return self.mock.mx

    def now(self):  # type: ignore[no-untyped-def]
        return self.mock.mx.clock.now()

    async def top(self, exchange_id: str) -> tuple[int, int]:
        """(best bid, best ask) in pips from the mock book."""
        book = await self.mock.client.get_orderbook(exchange_id, self.mock.tid)
        return to_pips(book.bids[0].price), to_pips(book.asks[0].price)

    async def fair(self, exchange_id: str) -> int:
        return to_pips((await self.mock.state())["fair"][exchange_id])

    async def fake_sleep(self, seconds: float) -> None:
        """Retry/backoff sleeps advance the mock clock instead of wall time."""
        self.sleeps.append(seconds)
        self.mock.mx.clock.advance(seconds)
        await asyncio.sleep(0)

    async def open_bridge(self, mode: Literal["paper", "live"] = "live", *, start: bool = True) -> Bridge:
        if mode == "live":
            (self.home / "ops").mkdir(exist_ok=True)
            (self.home / "ops" / "GO_LIVE").write_text("go live\n")
        bridge = await Bridge.open(
            self.config,
            transport=httpx.ASGITransport(app=self.mock.app),
            base_url="http://mock/api/v1",
            mode=mode,
            clock=self.mx.clock.now,
            sleep=self.fake_sleep,
        )
        if start:
            await bridge.start()
        return bridge

    async def write_market_map(
        self, *, verified: bool = True, independent: set[str] | None = None, only: set[str] | None = None
    ) -> None:
        state = await self.mock.state()
        independent = independent if independent is not None else {"1315", "1316"}
        mm = ops_pb2.MarketMap(header=header("test", 1, now_ns=int(self.now().timestamp() * 1e9), sha="t"))
        for m in state["markets"]:
            ex = str(m["exchangeId"])
            if only is not None and ex not in only:
                continue
            st = exchange_state(ex)
            clusters = ["national"] + ([f"state:{st}"] if st else ["house"])
            mm.markets.append(
                ops_pb2.MarketRule(
                    market_id=str(m["marketId"]),
                    exchange_id=ex,
                    title=m["title"],
                    race_id=f"2026-SEN-{st}" if st else "",
                    kind="race_party" if st else "chamber",
                    independent_can_win=ex in independent,
                    clusters=clusters,
                    verified=verified,
                )
            )
        self.config.market_map_path.parent.mkdir(parents=True, exist_ok=True)
        self.config.market_map_path.write_text(to_json(mm))

    async def write_fair(self, overrides: dict[str, float] | None = None, *, age_s: float = 0.0) -> None:
        state = await self.mock.state()
        produced = int((self.now().timestamp() - age_s) * 1e9)
        tf = targets_pb2.TargetFile(
            header=common_pb2.Header(schema_version=1, produced_at_ns=produced, seq=1)
        )
        fair = {**state["fair"], **(overrides or {})}
        for ex, p in fair.items():
            tf.fair.append(targets_pb2.FairValue(exchange_id=str(ex), p_yes=float(p), source="test"))
        self.config.fair_path.parent.mkdir(parents=True, exist_ok=True)
        self.config.fair_path.write_text(to_json(tf))


def make_home(tmp_path: Path) -> Path:
    home = tmp_path / "home"
    (home / "config").mkdir(parents=True)
    shutil.copy(REPO / "config" / "susq.toml", home / "config" / "susq.toml")
    return home


@pytest.fixture
def home(tmp_path: Path) -> Path:
    return make_home(tmp_path)


@pytest.fixture
def env_vars() -> dict[str, str]:
    return {"SUSQ_API_KEY": KEY, "SUSQ_MODE": "paper"}


@pytest.fixture
async def harness(home: Path, env_vars: dict[str, str]) -> AsyncIterator[Harness]:
    async with open_mock_env(MockConfig(seed=7, frozen_now=T0)) as mock:
        config = load_config(home, env=env_vars)
        yield Harness(mock, home, config, [])


@pytest.fixture
async def live_bridge(harness: Harness) -> AsyncIterator[Bridge]:
    bridge = await harness.open_bridge("live")
    try:
        yield bridge
    finally:
        await bridge.close()


@pytest.fixture
async def paper_bridge(harness: Harness) -> AsyncIterator[Bridge]:
    bridge = await harness.open_bridge("paper")
    try:
        yield bridge
    finally:
        await bridge.close()
