"""Assembly and lifecycle of one bridge process.

``Bridge.open`` wires config -> journal -> client -> kill switch/mode -> lease -> intents -> limiter ->
tournament -> risk -> gateway -> OMS -> market state. ``Bridge.start`` then does what every restart
must do before acting: take the lease (or stay read-only), load markets/rules/fair values, reconcile
from REST, and resume or close out unresolved intents. ``Bridge.run`` supervises the long-lived loops.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import random
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any, Literal

import httpx
from susq.platform.client import SuperMarketClient
from susq.platform.models import RealtimeToken, Tournament
from susq.platform.realtime import BroadcastMeta, RealtimeClient

from susq_bridge.config import BridgeConfig
from susq_bridge.gateway import Gateway
from susq_bridge.intents import IntentStore
from susq_bridge.journal import Journal
from susq_bridge.killswitch import KillSwitch, resolve_mode
from susq_bridge.lease import Lease
from susq_bridge.limiter import AdaptiveLimiter
from susq_bridge.market_state import MarketState
from susq_bridge.oms import Oms
from susq_bridge.recorder import Recorder
from susq_bridge.risk import RiskEngine
from susq_bridge.status import StatusWriter
from susq_bridge.strategies.racer import Racer
from susq_bridge.strategies.sniper import Sniper

__all__ = ["Bridge", "BridgeError", "Loops"]

log = logging.getLogger(__name__)


class BridgeError(RuntimeError):
    """The bridge cannot start safely (no key, unresolvable tournament, ...)."""


@dataclass
class Loops:
    racer: bool = True
    sniper: bool = True
    recorder: bool = True
    realtime: bool = True
    sniper_interval_s: float = 5.0
    status_interval_s: float = 60.0


@dataclass
class Bridge:
    config: BridgeConfig
    client: SuperMarketClient
    journal: Journal
    kill: KillSwitch
    lease: Lease
    intents: IntentStore
    limiter: AdaptiveLimiter
    tournament: Tournament
    risk: RiskEngine
    gateway: Gateway
    oms: Oms
    market: MarketState
    recorder: Recorder
    status: StatusWriter
    racer: Racer
    sniper: Sniper
    clock: Callable[[], datetime]
    realtime: RealtimeClient | None = None
    tasks: list[asyncio.Task[Any]] = field(default_factory=list)
    events: list[str] = field(default_factory=list)

    @property
    def mode(self) -> Literal["paper", "live"]:
        return self.gateway.mode

    @classmethod
    async def open(
        cls,
        config: BridgeConfig,
        *,
        transport: httpx.AsyncBaseTransport | None = None,
        base_url: str | None = None,
        mode: Literal["paper", "live"] | None = None,
        clock: Callable[[], datetime] = lambda: datetime.now(UTC),
        sleep: Callable[[float], Awaitable[None]] = asyncio.sleep,
        rng: random.Random | None = None,
    ) -> Bridge:
        if not config.api_key:
            raise BridgeError("SUSQ_API_KEY is not set (put it in .env); nothing can run without it")
        journal = Journal(config.journal_dir, "bridge", secrets=[config.api_key], clock=clock)
        client = SuperMarketClient(
            base_url or config.platform.base_url,
            config.api_key,
            timeout_s=config.platform.request_timeout_ms / 1000,
            journal=journal.record_http,
            transport=transport,
        )
        kill = KillSwitch(config.kill_file, config.go_live_file)
        resolved = mode or resolve_mode(config.mode_default, config.env, kill)
        if resolved == "live" and not kill.go_live_armed():
            raise BridgeError("live mode requires ops/GO_LIVE containing 'go live'")

        def clock_ns() -> int:  # one clock for every component (tests drive the mock's)
            return int(clock().timestamp() * 1e9)

        lease = Lease(config.lease_path, "bridge", ttl_s=config.lease_ttl_s, clock_ns=clock_ns)
        intents = IntentStore(config.intents_path, config.oms.idempotency_namespace, clock_ns=clock_ns)
        plat = config.platform
        limiter = AdaptiveLimiter(plat.initial_rps, plat.min_rps, plat.max_rps, plat.order_budget_frac)
        try:
            await limiter.acquire("read")
            tournament = await client.get_tournament(plat.tournament_slug)
        except Exception as exc:
            await client.aclose()
            raise BridgeError(f"cannot resolve tournament {plat.tournament_slug!r}: {exc}") from exc
        risk = RiskEngine(config.risk, tournament.id, clock=clock)
        gateway = Gateway(
            config,
            client=client,
            mode=resolved,
            tournament_id=tournament.id,
            journal=journal,
            intents=intents,
            lease=lease,
            kill=kill,
            risk=risk,
            limiter=limiter,
            clock=clock,
            sleep=sleep,
            rng=rng,
            paper_cash=tournament.my_balance or tournament.initial_balance or 100_000.0,
        )
        oms = Oms(clock=clock)
        market = MarketState(gateway, config, clock=clock)
        market.tournament = tournament
        oms.attach(gateway, market)
        bridge = cls(
            config=config,
            client=client,
            journal=journal,
            kill=kill,
            lease=lease,
            intents=intents,
            limiter=limiter,
            tournament=tournament,
            risk=risk,
            gateway=gateway,
            oms=oms,
            market=market,
            recorder=Recorder(config.data_dir, clock=clock),
            status=StatusWriter(config.status_path, clock=clock),
            racer=Racer(gateway, market, config.racer, clock=clock),
            sniper=Sniper(gateway, market, config.sniper, clock=clock),
            clock=clock,
        )
        journal.event(
            "bridge_open", {"mode": resolved, "tournamentId": tournament.id, "slug": tournament.slug}
        )
        return bridge

    async def close(self) -> None:
        for task in self.tasks:
            task.cancel()
        for task in self.tasks:
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await task
        self.tasks.clear()
        if self.realtime is not None:
            await self.realtime.stop()
        await self.client.aclose()
        self.journal.close()

    # ── startup ─────────────────────────────────────────────────────────────────────────────

    async def start(self) -> bool:
        """Load state and reconcile before acting. Returns True if we hold the trader lease."""
        trader = self.lease.acquire()
        # Renew from the moment we take it: the open sequence (load, reconcile, measure-day1) can outlast
        # the 30 s TTL when the platform is slow, and a lapsed lease used to stay lost for good.
        self._hold_lease()
        if not trader:
            current = self.lease.read()
            self.events.append(f"read-only: lease held by {current.holder if current else '?'}")
        await self.market.load_markets()
        rules = self.market.load_market_map()
        self.risk.clusters = {ex: list(r.clusters) for ex, r in self.market.rules.items()}
        await self.market.refresh_prices()
        self.market.load_fair()
        await self.oms.reconcile()
        if trader:
            await self.gateway.resume_intents()
            await self.oms.reconcile()
        self.journal.event(
            "bridge_start",
            {
                "trader": trader,
                "markets": len(self.market.markets),
                "rules": rules,
                "equity": self.oms.equity,
            },
        )
        return trader

    # ── realtime ────────────────────────────────────────────────────────────────────────────

    async def _token(self) -> RealtimeToken:
        return await self.gateway.read("realtime_token", self.client.mint_realtime_token)

    def build_realtime(self, first_token: RealtimeToken | None = None) -> RealtimeClient:
        cached = [first_token] if first_token is not None else []

        async def provider() -> RealtimeToken:
            return cached.pop() if cached else await self._token()

        rt = RealtimeClient(provider, on_resync_needed=self.oms.request_resync)
        tid = self.tournament.id
        if self.config.racer.enabled:
            rt.subscribe(f"relationships:violations:{tid}", "violation", self.racer.on_violation)
        rt.subscribe(f"tournament:{tid}", "market_batch", self._on_market_batch)
        if first_token is not None and (user := first_token.channels.get("user")):
            rt.subscribe(user, "account_batch", self._on_account_batch)
        self.realtime = rt
        return rt

    def _on_market_batch(self, payload: dict[str, Any], meta: BroadcastMeta) -> None:
        for item in payload.get("bookDirty") or []:
            if isinstance(item, dict) and (ex := item.get("exchangeId")):
                self.market.invalidate_book(str(ex))

    def _on_account_batch(self, payload: dict[str, Any], meta: BroadcastMeta) -> None:
        if payload.get("fills") or payload.get("orderUpdates") or payload.get("settlements"):
            self.oms.request_resync("account_batch", meta.channel)

    # ── loops ───────────────────────────────────────────────────────────────────────────────

    def _spawn(self, coro: Awaitable[Any], name: str) -> None:
        self.tasks.append(asyncio.ensure_future(coro))
        self.tasks[-1].set_name(name)

    async def _every(self, interval_s: float, fn: Callable[[], Awaitable[Any]], name: str) -> None:
        while True:
            try:
                await fn()
            except Exception:
                log.exception("%s failed", name)
            await asyncio.sleep(interval_s)

    async def _watch_kill(self) -> None:
        while True:
            if self.kill.killed():
                await self.gateway.enforce_kill()
            await asyncio.sleep(1.0)

    async def _lock_gate(self) -> None:
        """Cancel everything at calendar.last_order_utc (16:55Z Nov 4); the time gate blocks new orders."""
        wait = (self.config.calendar.last_order_utc - self.clock()).total_seconds()
        if wait > 0:
            await asyncio.sleep(wait)
        await self.gateway.cancel_all("lock: last_order_utc reached")

    def _hold_lease(self) -> None:
        if any(task.get_name() == "lease" and not task.done() for task in self.tasks):
            return
        self._spawn(
            self.lease.hold(self.config.lease_renew_s, self._on_lease_lost, self._on_lease_regained), "lease"
        )

    async def _on_lease_lost(self) -> None:
        self.journal.event("lease_lost", {})
        self.events.append("lease lost: trading paused (gateway refuses writes)")

    async def _on_lease_regained(self) -> None:
        self.journal.event("lease_regained", {})
        self.events.append("lease regained: trading resumed")
        try:
            await self.gateway.resume_intents()
            await self.oms.reconcile()
        except Exception:
            log.exception("resync after regaining the lease failed; the reconcile loop retries")

    async def run(self, loops: Loops | None = None) -> None:
        """Run until cancelled. Strategies only act while this process holds the lease."""
        loops = loops or Loops()
        cfg = self.config
        self._hold_lease()
        self._spawn(self._watch_kill(), "kill")
        self._spawn(self._lock_gate(), "lock-gate")
        self._spawn(self.oms.run(cfg.oms.reconcile_interval_s, cfg.fair.stale_after_s), "reconcile")
        self._spawn(self._every(loops.status_interval_s, self.write_status, "status"), "status")
        if loops.realtime:
            try:
                first = await self._token()
            except Exception:
                log.exception("realtime token mint failed; polling only")
                first = None
            rt = self.build_realtime(first)
            self._spawn(rt.run(), "realtime")
        if loops.racer and cfg.racer.enabled:
            await self.racer.load_relationships()
            self._spawn(self.racer.run_polling(self.realtime_up), "racer-poll")
        if loops.sniper and cfg.sniper.enabled:
            self._spawn(self._every(loops.sniper_interval_s, self.sniper.scan, "sniper"), "sniper")
        if loops.recorder:
            await self.recorder.snapshot_markets(self.gateway, self.market)
            self._spawn(
                self._every(
                    cfg.price_snapshot_interval_s, lambda: self.recorder.record_prices(self.market), "prices"
                ),
                "recorder-prices",
            )
            self._spawn(
                self._every(
                    cfg.leaderboard_interval_s,
                    lambda: self.recorder.record_leaderboard(self.gateway),
                    "leaderboard",
                ),
                "recorder-leaderboard",
            )
        await asyncio.gather(*self.tasks)

    def realtime_up(self) -> bool:
        return self.realtime is not None and self.realtime.connected.is_set()

    async def write_status(self) -> None:
        self.status.write(self)
