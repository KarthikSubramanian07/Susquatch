"""Test harness around the mock exchange, shared by ``py/tests`` and ``bridge_py/tests``.

* :func:`open_mock_env` - the mock in-process over ``httpx.ASGITransport`` plus a real
  :class:`~susq.platform.client.SuperMarketClient` and an admin client. Fast; no sockets.
* :func:`serve_mock` - the mock on a real TCP port via uvicorn (needed for websockets).
"""

from __future__ import annotations

import asyncio
import contextlib
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from typing import Any

import httpx
import uvicorn
from starlette.applications import Starlette

from susq.mock.server import MockConfig, MockExchange, create_app
from susq.platform.client import HttpRecord, SuperMarketClient
from susq.platform.models import OrderRequest

__all__ = ["KEY", "SLUG", "T0", "LiveMock", "MockEnv", "open_mock_env", "serve_mock"]

SLUG = "midterm-elections"
KEY = "mock-key-alpha"
#: Trading opens 2026-10-01 16:00Z; tests freeze the mock clock there.
T0 = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)


@dataclass
class MockEnv:
    app: Starlette
    client: SuperMarketClient
    admin: httpx.AsyncClient
    tid: str
    records: list[HttpRecord] = field(default_factory=list)

    @property
    def mx(self) -> MockExchange:
        mx: MockExchange = self.app.state.mx
        return mx

    @property
    def transport(self) -> httpx.ASGITransport:
        return httpx.ASGITransport(app=self.app)

    def order(
        self,
        exchange_id: str,
        side: str = "yes",
        action: str = "buy",
        qty: int = 10,
        price_pips: int = 5000,
        ttl_s: int = 300,
    ) -> OrderRequest:
        return OrderRequest(
            exchange_id=exchange_id,
            side=side,  # type: ignore[arg-type]
            action=action,  # type: ignore[arg-type]
            quantity=qty,
            price_pips=price_pips,
            expiration=self.mx.clock.now() + timedelta(seconds=ttl_s),
            tournament_id=self.tid,
        )

    async def state(self) -> dict[str, Any]:
        data: dict[str, Any] = (await self.admin.get("/_mock/state")).json()
        return data

    async def post_admin(self, path: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
        resp = await self.admin.post(path, json=body or {})
        if resp.status_code != 200:
            raise AssertionError(f"{path}: {resp.status_code} {resp.text}")
        data: dict[str, Any] = resp.json()
        return data


@contextlib.asynccontextmanager
async def open_mock_env(config: MockConfig | None = None) -> AsyncIterator[MockEnv]:
    app = create_app(config or MockConfig(seed=7, frozen_now=T0))
    transport = httpx.ASGITransport(app=app)
    records: list[HttpRecord] = []
    client = SuperMarketClient("http://mock/api/v1", KEY, transport=transport, journal=records.append)
    admin = httpx.AsyncClient(transport=transport, base_url="http://mock")
    try:
        yield MockEnv(app, client, admin, app.state.mx.tid, records)
    finally:
        await client.aclose()
        await admin.aclose()


@dataclass
class LiveMock:
    """The mock on a real TCP port."""

    app: Starlette
    base: str

    @property
    def mx(self) -> MockExchange:
        mx: MockExchange = self.app.state.mx
        return mx

    @property
    def api(self) -> str:
        return f"{self.base}/api/v1"


@contextlib.asynccontextmanager
async def serve_mock(config: MockConfig | None = None) -> AsyncIterator[LiveMock]:
    config = config or MockConfig(seed=7)
    app = create_app(config)
    server = uvicorn.Server(
        uvicorn.Config(app, host="127.0.0.1", port=0, log_level="warning", lifespan="off")
    )
    task = asyncio.create_task(server.serve())
    while not server.started:
        if task.done():
            task.result()
        await asyncio.sleep(0.01)
    port = server.servers[0].sockets[0].getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    config.public_origin = base
    try:
        yield LiveMock(app, base)
    finally:
        server.should_exit = True
        with contextlib.suppress(asyncio.CancelledError, TimeoutError):
            await asyncio.wait_for(task, timeout=5)
