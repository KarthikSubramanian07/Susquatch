"""A small Supabase Realtime look-alike: the Phoenix channel protocol (``vsn=1.0.0`` JSON frames).

Frames are JSON objects ``{"topic", "event", "payload", "ref", "join_ref"}``. Supported:

* ``phoenix``/``heartbeat`` -> ``phx_reply`` ``{"status": "ok"}``.
* ``realtime:<channel>``/``phx_join`` with ``payload.access_token`` and
  ``payload.config.private == true`` -> ``phx_reply`` ok, or ``error`` with a ``reason``.
* ``realtime:<channel>``/``access_token`` refreshes the channel's token; an invalid token closes the
  channel (``system`` error + ``phx_close``), like Supabase does.
* ``phx_leave``.
* Server broadcasts: ``{"event": "broadcast", "payload": {"type": "broadcast", "event": <name>,
  "payload": {...}}}``. ``market_batch``/``account_batch`` carry ``delivery.revision`` and
  ``delivery.previousRevision``; the hub can be told to skip a revision to simulate a missed batch.

Tokens are validated through a callback so the mock's ``POST /realtime/token`` stays the authority.
"""

from __future__ import annotations

import contextlib
import uuid
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

import orjson
from starlette.websockets import WebSocket, WebSocketDisconnect

__all__ = ["PhoenixHub", "TokenInfo"]

REVISIONED_EVENTS = frozenset({"market_batch", "account_batch"})


@dataclass(frozen=True, slots=True)
class TokenInfo:
    profile_id: str
    expires_at: datetime


@dataclass(eq=False)
class _Conn:
    ws: WebSocket
    #: topic ("realtime:<channel>") -> (join_ref, token)
    topics: dict[str, tuple[str | None, str]] = field(default_factory=dict)


class PhoenixHub:
    def __init__(
        self,
        resolve_token: Callable[[str], TokenInfo | None],
        now: Callable[[], datetime],
        *,
        anon_key: str = "mock-anon-key",
    ) -> None:
        self._resolve = resolve_token
        self._now = now
        self.anon_key = anon_key
        self._conns: set[_Conn] = set()
        self._revisions: dict[str, int] = {}
        self.joins = 0  # observable counter for tests

    # ── connection handling ─────────────────────────────────────────────────────────────────

    async def serve(self, ws: WebSocket) -> None:
        if ws.query_params.get("apikey") != self.anon_key:
            await ws.close(code=4001)
            return
        await ws.accept()
        conn = _Conn(ws)
        self._conns.add(conn)
        try:
            while True:
                text = await ws.receive_text()
                try:
                    msg = orjson.loads(text)
                except orjson.JSONDecodeError:
                    continue
                await self._handle(conn, msg)
        except WebSocketDisconnect:
            pass
        finally:
            self._conns.discard(conn)

    async def _send(self, conn: _Conn, frame: dict[str, Any]) -> None:
        try:
            await conn.ws.send_text(orjson.dumps(frame).decode())
        except Exception:  # peer went away mid-send; the receive loop cleans up
            self._conns.discard(conn)

    async def _reply(self, conn: _Conn, msg: dict[str, Any], status: str, response: dict[str, Any]) -> None:
        await self._send(
            conn,
            {
                "topic": msg.get("topic"),
                "event": "phx_reply",
                "payload": {"status": status, "response": response},
                "ref": msg.get("ref"),
                "join_ref": msg.get("join_ref"),
            },
        )

    def _authorize(self, channel: str, token: str) -> str | None:
        """Return an error reason, or None if the token may read ``channel``."""
        info = self._resolve(token)
        if info is None:
            return "Invalid token"
        if info.expires_at <= self._now():
            return "Token has expired"
        if channel.startswith("user:") and channel.removeprefix("user:") != info.profile_id:
            return "Unauthorized: user channel belongs to another profile"
        return None

    async def _handle(self, conn: _Conn, msg: dict[str, Any]) -> None:
        topic = str(msg.get("topic", ""))
        event = msg.get("event")
        payload = msg.get("payload") or {}
        if topic == "phoenix" and event == "heartbeat":
            await self._reply(conn, msg, "ok", {})
            return
        if not topic.startswith("realtime:"):
            await self._reply(conn, msg, "error", {"reason": "unknown topic"})
            return
        channel = topic.removeprefix("realtime:")
        if event == "phx_join":
            config = payload.get("config") or {}
            token = payload.get("access_token") or payload.get("user_token") or ""
            if not config.get("private"):
                await self._reply(conn, msg, "error", {"reason": "private channel required"})
                return
            reason = self._authorize(channel, str(token))
            if reason is not None:
                await self._reply(conn, msg, "error", {"reason": reason})
                return
            conn.topics[topic] = (msg.get("join_ref") or msg.get("ref"), str(token))
            self.joins += 1
            await self._reply(conn, msg, "ok", {"postgres_changes": []})
        elif event == "access_token":
            token = str(payload.get("access_token") or "")
            if topic not in conn.topics:
                return
            reason = self._authorize(channel, token)
            if reason is not None:
                await self._close_topic(conn, topic, reason)
                return
            conn.topics[topic] = (conn.topics[topic][0], token)
        elif event == "phx_leave":
            conn.topics.pop(topic, None)
            await self._reply(conn, msg, "ok", {})

    async def _close_topic(self, conn: _Conn, topic: str, reason: str) -> None:
        join_ref = conn.topics.pop(topic, (None, ""))[0]
        await self._send(
            conn,
            {
                "topic": topic,
                "event": "system",
                "payload": {"status": "error", "message": reason, "extension": "system"},
                "ref": None,
                "join_ref": join_ref,
            },
        )
        await self._send(conn, {"topic": topic, "event": "phx_close", "payload": {}, "ref": join_ref})

    # ── server-side control ─────────────────────────────────────────────────────────────────

    async def broadcast(self, channel: str, event: str, payload: dict[str, Any]) -> int:
        """Send one broadcast to every subscriber of ``channel``. Returns deliveries."""
        body = dict(payload)
        if event in REVISIONED_EVENTS:
            rev = self._revisions.get(channel, 0) + 1
            self._revisions[channel] = rev
            body["delivery"] = {
                "revision": rev,
                "previousRevision": rev - 1,
                "correlationId": uuid.uuid4().hex,
            }
        topic = f"realtime:{channel}"
        frame = {
            "topic": topic,
            "event": "broadcast",
            "payload": {"type": "broadcast", "event": event, "payload": body},
            "ref": None,
        }
        sent = 0
        for conn in list(self._conns):
            joined = conn.topics.get(topic)
            if joined is None:
                continue
            reason = self._authorize(channel, joined[1])
            if reason is not None:  # token expired while subscribed
                await self._close_topic(conn, topic, reason)
                continue
            await self._send(conn, frame)
            sent += 1
        return sent

    def skip_revision(self, channel: str, count: int = 1) -> None:
        """Burn revisions without sending: the next batch arrives with a gap."""
        self._revisions[channel] = self._revisions.get(channel, 0) + count

    async def disconnect_all(self) -> int:
        conns = list(self._conns)
        for conn in conns:
            with contextlib.suppress(Exception):
                await conn.ws.close(code=1012)
            self._conns.discard(conn)
        return len(conns)

    @property
    def connections(self) -> int:
        return len(self._conns)
