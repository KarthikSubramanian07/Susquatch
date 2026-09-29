"""RealtimeClient against the mock's Phoenix endpoint on a real socket."""

from __future__ import annotations

import asyncio
import contextlib
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from typing import Any

import httpx
import pytest
from susq.mock.testing import KEY, LiveMock
from susq.platform.client import SuperMarketClient
from susq.platform.models import RealtimeToken
from susq.platform.realtime import BroadcastMeta, RealtimeClient, websocket_url


@dataclass
class Harness:
    live: LiveMock
    client: SuperMarketClient
    admin: httpx.AsyncClient
    rt: RealtimeClient
    tid: str
    events: list[tuple[str, dict[str, Any]]] = field(default_factory=list)
    resyncs: list[tuple[str, str | None]] = field(default_factory=list)
    mints: int = 0

    async def wait_for(self, predicate: Callable[[], bool], timeout: float = 5.0) -> None:
        deadline = asyncio.get_running_loop().time() + timeout
        while not predicate():
            if asyncio.get_running_loop().time() > deadline:
                raise AssertionError("condition not met in time")
            await asyncio.sleep(0.02)


@contextlib.asynccontextmanager
async def harness(
    live: LiveMock,
    *,
    provider: Callable[[SuperMarketClient], Awaitable[RealtimeToken]] | None = None,
    **kw: Any,
) -> AsyncIterator[Harness]:
    client = SuperMarketClient(live.api, KEY)
    admin = httpx.AsyncClient(base_url=live.base)
    tid = live.mx.tid
    h: Harness

    async def mint() -> RealtimeToken:
        h.mints += 1
        return await (provider(client) if provider else client.mint_realtime_token())

    def on_resync(reason: str, channel: str | None) -> None:
        h.resyncs.append((reason, channel))

    rt = RealtimeClient(mint, on_resync_needed=on_resync, backoff_initial_s=0.05, backoff_max_s=0.2, **kw)
    h = Harness(live, client, admin, rt, tid)

    def record(payload: dict[str, Any], meta: BroadcastMeta) -> None:
        h.events.append((meta.event, payload))

    rt.subscribe(f"relationships:violations:{tid}", "violation", record)
    rt.subscribe(f"tournament:{tid}", "market_batch", record)
    task = asyncio.create_task(rt.run())
    try:
        yield h
    finally:
        await rt.stop()
        with contextlib.suppress(asyncio.CancelledError, TimeoutError):
            await asyncio.wait_for(task, timeout=5)
        await client.aclose()
        await admin.aclose()


def test_websocket_url() -> None:
    assert (
        websocket_url("https://abc.supabase.co", "anon")
        == "wss://abc.supabase.co/realtime/v1/websocket?apikey=anon&vsn=1.0.0"
    )
    assert websocket_url("http://127.0.0.1:9/", "k").startswith("ws://127.0.0.1:9/realtime/v1/websocket?")


async def test_joins_and_receives_violations(live_mock: LiveMock) -> None:
    async with harness(live_mock) as h:
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        assert h.resyncs[0] == ("connect", None)
        assert live_mock.mx.hub.joins == 2
        r = await h.admin.post("/_mock/violation", json={"amount": 0.06})
        rel_id = r.json()["relationshipId"]
        await h.wait_for(lambda: any(e == "violation" for e, _ in h.events))
        violation = next(p for e, p in h.events if e == "violation")
        assert violation["relationshipId"] == rel_id
        assert violation["evaluationStatus"] == "violated"
        assert any(e == "market_batch" for e, _ in h.events)


async def test_gap_triggers_resync_and_duplicates_are_dropped(live_mock: LiveMock) -> None:
    async with harness(live_mock) as h:
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        channel = f"tournament:{h.tid}"
        payload = {"trades": [], "bookDirty": [], "marketSettled": []}
        await h.admin.post(
            "/_mock/realtime/broadcast",
            json={"channel": channel, "event": "market_batch", "payload": payload},
        )
        await h.wait_for(lambda: h.rt.stats.messages >= 1)
        await h.admin.post("/_mock/realtime/skip-revision", json={"channel": channel})
        await h.admin.post(
            "/_mock/realtime/broadcast",
            json={"channel": channel, "event": "market_batch", "payload": payload},
        )
        await h.wait_for(lambda: ("gap", channel) in h.resyncs)
        assert h.rt.stats.gaps == 1
        # A replayed (duplicate) revision is ignored.
        hub = live_mock.mx.hub
        hub._revisions[channel] -= 1  # the next broadcast reuses the last revision
        before = h.rt.stats.messages
        await h.admin.post(
            "/_mock/realtime/broadcast",
            json={"channel": channel, "event": "market_batch", "payload": payload},
        )
        await h.wait_for(lambda: h.rt.stats.duplicates == 1)
        assert h.rt.stats.messages == before


async def test_reconnects_and_resyncs_after_disconnect(live_mock: LiveMock) -> None:
    async with harness(live_mock) as h:
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        await h.admin.post("/_mock/realtime/disconnect")
        await h.wait_for(lambda: ("reconnect", None) in h.resyncs)
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        assert h.rt.stats.reconnects >= 1
        await h.admin.post("/_mock/violation", json={"amount": 0.06})
        await h.wait_for(lambda: any(e == "violation" for e, _ in h.events))


async def test_token_refresh_before_expiry(live_mock: LiveMock) -> None:
    # Refresh when fewer than (3 h - 1 s) remain: i.e. about one second after joining.
    async with harness(live_mock, refresh_before_s=3 * 3600 - 1) as h:
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        await h.wait_for(lambda: h.rt.stats.token_refreshes >= 1, timeout=5)
        assert ("token_refresh", None) in h.resyncs
        assert h.mints >= 2
        await h.admin.post("/_mock/violation", json={"amount": 0.06})
        await h.wait_for(lambda: any(e == "violation" for e, _ in h.events))


async def test_expired_token_gets_kicked_and_client_recovers(live_mock: LiveMock) -> None:
    async with harness(live_mock) as h:
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        live_mock.mx.clock.advance(3 * 3600 + 1)  # every minted token is now expired server-side
        channel = f"tournament:{h.tid}"
        await h.admin.post(
            "/_mock/realtime/broadcast", json={"channel": channel, "event": "market_batch", "payload": {}}
        )
        await h.wait_for(lambda: h.rt.stats.reconnects >= 1)
        assert h.mints >= 2  # it re-minted rather than reusing the dead token


async def test_bad_token_join_is_refused_then_retried(live_mock: LiveMock) -> None:
    attempts = {"n": 0}

    async def provider(client: SuperMarketClient) -> RealtimeToken:
        attempts["n"] += 1
        token = await client.mint_realtime_token()
        if attempts["n"] == 1:
            return token.model_copy(update={"token": "forged-token"})
        return token

    async with harness(live_mock, provider=provider) as h:
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        assert "refused" in (h.rt.stats.last_error or "")
        assert attempts["n"] >= 2


async def test_heartbeats_keep_the_session_alive(live_mock: LiveMock) -> None:
    async with harness(live_mock, heartbeat_s=0.05) as h:
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        await asyncio.sleep(0.4)
        assert h.rt.connected.is_set()
        assert h.rt.stats.reconnects == 0


async def test_callback_errors_do_not_kill_the_session(live_mock: LiveMock) -> None:
    async with harness(live_mock) as h:

        def boom(payload: dict[str, Any], meta: BroadcastMeta) -> None:
            raise RuntimeError("strategy bug")

        h.rt._callbacks[(f"tournament:{h.tid}", "market_batch")].insert(0, boom)
        await asyncio.wait_for(h.rt.connected.wait(), 5)
        await h.admin.post("/_mock/violation", json={"amount": 0.06})
        await h.wait_for(lambda: any(e == "violation" for e, _ in h.events))
        assert h.rt.stats.callback_errors >= 1
        assert h.rt.connected.is_set()


@pytest.mark.parametrize("expired_minutes", [-1])
def test_seconds_left_helper(expired_minutes: int) -> None:
    tok = RealtimeToken(
        token="t",
        expires_at=datetime.now(UTC) + timedelta(minutes=expired_minutes),
        supabase_url="http://x",
        anon_key="a",
    )
    assert RealtimeClient._seconds_left(tok) < 0
