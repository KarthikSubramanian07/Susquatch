"""Supabase Realtime client (Phoenix channel protocol over ``websockets``) for The Super Market feeds.

Protocol (``vsn=1.0.0``, JSON object frames ``{topic, event, payload, ref, join_ref}``):

* connect to ``{supabaseUrl}/realtime/v1/websocket?apikey={anonKey}&vsn=1.0.0`` (http -> ws);
* join each private channel on topic ``realtime:<channel>`` with ``phx_join`` and payload
  ``{config: {broadcast: {ack: false, self: false}, presence: {key: ""}, postgres_changes: [],
  private: true}, access_token}``;
* heartbeat on topic ``phoenix`` (event ``heartbeat``) and expect a ``phx_reply``;
* refresh the JWT before its 3 h expiry by sending ``access_token`` on every joined topic;
* server broadcasts arrive as ``event: "broadcast"`` with ``payload: {type, event, payload}``.

Reliability contract (from the official docs): delivery is best effort with no replay. Clients must
resynchronize from REST on initial subscription, reconnect, token refresh, socket error, and on any
``delivery.previousRevision`` that doesn't match the last accepted revision. This client calls
``on_resync_needed(reason, channel)`` in every one of those cases, and drops duplicate revisions.
"""

from __future__ import annotations

import asyncio
import contextlib
import inspect
import itertools
import logging
import random
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any
from urllib.parse import urlencode, urlsplit, urlunsplit

import orjson
import websockets
from websockets.asyncio.client import ClientConnection

from susq.platform.models import RealtimeToken

__all__ = [
    "BroadcastMeta",
    "RealtimeClient",
    "RealtimeError",
    "RealtimeStats",
    "websocket_url",
]

log = logging.getLogger(__name__)

Callback = Callable[[dict[str, Any], "BroadcastMeta"], Awaitable[None] | None]
ResyncCallback = Callable[[str, str | None], Awaitable[None] | None]
TokenProvider = Callable[[], Awaitable[RealtimeToken]]


class RealtimeError(Exception):
    """A session-ending protocol failure (join refused, heartbeat lost, channel closed)."""


@dataclass(frozen=True, slots=True)
class BroadcastMeta:
    channel: str
    event: str
    received_at: float  # time.time()
    revision: int | None


@dataclass
class RealtimeStats:
    connects: int = 0
    reconnects: int = 0  # successful re-joins after a lost session
    gaps: int = 0
    duplicates: int = 0
    messages: int = 0
    token_refreshes: int = 0
    last_message_at: float | None = None
    last_error: str | None = None
    callback_errors: int = 0
    per_channel: dict[str, int] = field(default_factory=dict)


def websocket_url(supabase_url: str, anon_key: str) -> str:
    parts = urlsplit(supabase_url.rstrip("/"))
    scheme = {"https": "wss", "http": "ws"}.get(parts.scheme, parts.scheme)
    path = parts.path.rstrip("/") + "/realtime/v1/websocket"
    return urlunsplit((scheme, parts.netloc, path, urlencode({"apikey": anon_key, "vsn": "1.0.0"}), ""))


async def _maybe_await(value: Awaitable[None] | None) -> None:
    if inspect.isawaitable(value):
        await value


class RealtimeClient:
    """Joins private broadcast channels and keeps them joined, forever, until :meth:`stop`."""

    def __init__(
        self,
        token_provider: TokenProvider,
        *,
        on_resync_needed: ResyncCallback | None = None,
        heartbeat_s: float = 25.0,
        refresh_before_s: float = 900.0,
        join_timeout_s: float = 10.0,
        backoff_initial_s: float = 0.5,
        backoff_max_s: float = 30.0,
        rng: random.Random | None = None,
    ) -> None:
        self._token_provider = token_provider
        self._on_resync = on_resync_needed
        self._heartbeat_s = heartbeat_s
        self._refresh_before_s = refresh_before_s
        self._join_timeout_s = join_timeout_s
        self._backoff_initial_s = backoff_initial_s
        self._backoff_max_s = backoff_max_s
        self._rng = rng or random.Random()  # noqa: S311 - jitter, not crypto
        self._callbacks: dict[tuple[str, str], list[Callback]] = {}
        self._channels: list[str] = []
        self._token: RealtimeToken | None = None
        self._refs = itertools.count(1)
        self._pending: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._revisions: dict[str, int] = {}
        self._stop = asyncio.Event()
        self._ws: ClientConnection | None = None
        self.connected = asyncio.Event()
        self.stats = RealtimeStats()

    # ── public API ──────────────────────────────────────────────────────────────────────────

    def subscribe(self, channel: str, event: str, callback: Callback) -> None:
        """Register ``callback`` for ``event`` broadcasts on ``channel`` (call before :meth:`run`)."""
        if channel not in self._channels:
            self._channels.append(channel)
        self._callbacks.setdefault((channel, event), []).append(callback)

    async def stop(self) -> None:
        self._stop.set()
        if self._ws is not None:
            with contextlib.suppress(Exception):
                await self._ws.close()

    async def run(self) -> None:
        """Connect, join, and reconnect with jittered exponential backoff until stopped."""
        attempt = 0
        while not self._stop.is_set():
            try:
                token = await self._current_token()
                url = websocket_url(token.supabase_url, token.anon_key)
                connect = websockets.connect(url, open_timeout=self._join_timeout_s, ping_interval=None)
                async with connect as ws:
                    self._ws = ws
                    await self._session(ws, reason="reconnect" if self.stats.connects else "connect")
                    attempt = 0
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # any failure ends the session; we always come back
                self.stats.last_error = f"{type(exc).__name__}: {exc}"
                log.warning("realtime session ended: %s", self.stats.last_error)
                if isinstance(exc, RealtimeError) and "token" in str(exc).lower():
                    self._token = None  # force a fresh mint on the next attempt
            finally:
                self._ws = None
                self.connected.clear()
                self._fail_pending()
            if self._stop.is_set():
                break
            delay = min(self._backoff_max_s, self._backoff_initial_s * 2**attempt)
            attempt += 1
            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(self._stop.wait(), timeout=delay * self._rng.uniform(0.5, 1.0))

    # ── session ─────────────────────────────────────────────────────────────────────────────

    async def _current_token(self) -> RealtimeToken:
        if self._token is None or self._seconds_left(self._token) <= self._refresh_before_s:
            self._token = await self._token_provider()
        return self._token

    @staticmethod
    def _seconds_left(token: RealtimeToken) -> float:
        return (token.expires_at - datetime.now(UTC)).total_seconds()

    async def _session(self, ws: ClientConnection, *, reason: str) -> None:
        self._revisions.clear()
        reader = asyncio.create_task(self._reader(ws), name="realtime-reader")
        try:
            for channel in self._channels:
                await self._join(ws, channel)
            self.stats.connects += 1
            if reason == "reconnect":
                self.stats.reconnects += 1
            self.connected.set()
            await self._resync(reason, None)
            heartbeat = asyncio.create_task(self._heartbeat(ws), name="realtime-heartbeat")
            refresher = asyncio.create_task(self._refresher(ws), name="realtime-refresh")
            stopper = asyncio.create_task(self._stop.wait(), name="realtime-stop")
            done, pending = await asyncio.wait(
                {reader, heartbeat, refresher, stopper}, return_when=asyncio.FIRST_COMPLETED
            )
            for task in pending:
                task.cancel()
            for task in pending:
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await task
            for task in done:
                if task is not stopper and (exc := task.exception()) is not None:
                    raise exc
        finally:
            if not reader.done():
                reader.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await reader

    def _fail_pending(self) -> None:
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(RealtimeError("connection closed"))
        self._pending.clear()

    async def _push(
        self, ws: ClientConnection, topic: str, event: str, payload: dict[str, Any], ref: str | None = None
    ) -> str:
        ref = ref or str(next(self._refs))
        frame: dict[str, Any] = {"topic": topic, "event": event, "payload": payload, "ref": ref}
        if event == "phx_join":
            frame["join_ref"] = ref
        await ws.send(orjson.dumps(frame).decode())
        return ref

    async def _request(
        self, ws: ClientConnection, topic: str, event: str, payload: dict[str, Any]
    ) -> dict[str, Any]:
        """Push and await the matching ``phx_reply`` (registered before sending: no lost replies)."""
        ref = str(next(self._refs))
        fut: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()
        self._pending[ref] = fut
        try:
            await self._push(ws, topic, event, payload, ref)
            return await asyncio.wait_for(fut, timeout=self._join_timeout_s)
        finally:
            self._pending.pop(ref, None)

    async def _join(self, ws: ClientConnection, channel: str) -> None:
        assert self._token is not None
        reply = await self._request(
            ws,
            f"realtime:{channel}",
            "phx_join",
            {
                "config": {
                    "broadcast": {"ack": False, "self": False},
                    "presence": {"key": ""},
                    "postgres_changes": [],
                    "private": True,
                },
                "access_token": self._token.token,
            },
        )
        if reply.get("status") != "ok":
            reason = (reply.get("response") or {}).get("reason", "join refused")
            raise RealtimeError(f"join {channel} refused: {reason}")

    async def _heartbeat(self, ws: ClientConnection) -> None:
        while True:
            await asyncio.sleep(self._heartbeat_s)
            try:
                await self._request(ws, "phoenix", "heartbeat", {})
            except TimeoutError as exc:
                raise RealtimeError("heartbeat timed out") from exc

    async def _refresher(self, ws: ClientConnection) -> None:
        while True:
            assert self._token is not None
            wait = self._seconds_left(self._token) - self._refresh_before_s
            await asyncio.sleep(max(0.0, wait))
            try:
                token = await self._token_provider()
            except Exception as exc:
                log.warning("realtime token refresh failed: %s", exc)
                await asyncio.sleep(min(30.0, max(1.0, self._seconds_left(self._token) / 10)))
                continue
            self._token = token
            for channel in self._channels:
                await self._push(ws, f"realtime:{channel}", "access_token", {"access_token": token.token})
            self.stats.token_refreshes += 1
            await self._resync("token_refresh", None)

    async def _reader(self, ws: ClientConnection) -> None:
        async for raw in ws:
            try:
                msg = orjson.loads(raw)
            except orjson.JSONDecodeError:
                continue
            if not isinstance(msg, dict):
                continue
            event = msg.get("event")
            topic = str(msg.get("topic") or "")
            if event == "phx_reply":
                fut = self._pending.get(str(msg.get("ref")))
                if fut is not None and not fut.done():
                    fut.set_result(msg.get("payload") or {})
            elif event == "broadcast":
                await self._dispatch(topic.removeprefix("realtime:"), msg.get("payload") or {})
            elif event in ("phx_close", "phx_error"):
                raise RealtimeError(f"{topic}: {event}")
            elif event == "system":
                payload = msg.get("payload") or {}
                if payload.get("status") == "error":
                    raise RealtimeError(f"{topic}: {payload.get('message', 'system error')} (token)")
        raise RealtimeError("socket closed by server")

    async def _dispatch(self, channel: str, envelope: dict[str, Any]) -> None:
        event = str(envelope.get("event") or "")
        payload = envelope.get("payload")
        if not isinstance(payload, dict):
            return
        delivery = payload.get("delivery") if isinstance(payload.get("delivery"), dict) else {}
        revision = delivery.get("revision") if isinstance(delivery.get("revision"), int) else None
        if revision is not None:
            last = self._revisions.get(channel)
            if last is not None and revision <= last:
                self.stats.duplicates += 1
                return
            previous = delivery.get("previousRevision")
            self._revisions[channel] = revision
            if last is not None and previous != last:
                self.stats.gaps += 1
                await self._resync("gap", channel)
        self.stats.messages += 1
        self.stats.last_message_at = time.time()
        self.stats.per_channel[channel] = self.stats.per_channel.get(channel, 0) + 1
        meta = BroadcastMeta(channel, event, time.time(), revision)
        for callback in self._callbacks.get((channel, event), []):
            try:
                await _maybe_await(callback(payload, meta))
            except Exception:
                self.stats.callback_errors += 1
                log.exception("realtime callback failed for %s/%s", channel, event)

    async def _resync(self, reason: str, channel: str | None) -> None:
        if self._on_resync is None:
            return
        try:
            await _maybe_await(self._on_resync(reason, channel))
        except Exception:
            self.stats.callback_errors += 1
            log.exception("resync callback failed (%s)", reason)
