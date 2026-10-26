"""``state/status.json``: a ``susq.v1.StatusSnapshot`` the status pusher ships to the dashboard.

Aggregates only (equity, stage, positions, recent trades, health). Written atomically every minute
by whichever process holds the lease.
"""

from __future__ import annotations

from collections.abc import Callable
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING

from susq.contracts import common_pb2, header, ops_pb2, to_json
from susq.platform.prices import PIPS_PER_UNIT

from susq_bridge.lease import atomic_write

if TYPE_CHECKING:
    from susq_bridge.runtime import Bridge

__all__ = ["StatusWriter", "cash_of", "stage_for"]

_STALK_FROM = datetime(2026, 10, 21, tzinfo=UTC)
_EMERGE_FROM = datetime(2026, 11, 3, 23, 0, tzinfo=UTC)  # 19:00 ET on election night


def stage_for(now: datetime, lock: datetime) -> str:
    if now >= lock:
        return "locked"
    if now >= _EMERGE_FROM:
        return "emerge"
    if now >= _STALK_FROM:
        return "stalk"
    return "forage"


def cash_of(bridge: Bridge) -> float:
    """Cash as risk sizes against it (EXP-015).

    Paper: the paper ledger. Live: the last portfolio read (account value minus holdings, see
    ``Gateway.portfolio``), kept by the risk engine. Only before the first read does it fall back to
    the tournament's start-up ``myBalance``, which the platform does not refresh for us.
    """
    if bridge.gateway.paper:
        return bridge.gateway.paper.ledger.cash
    if bridge.gateway.risk.cash is not None:
        return bridge.gateway.risk.cash
    return bridge.tournament.my_balance or 0.0


class StatusWriter:
    def __init__(self, path: Path, *, clock: Callable[[], datetime] = lambda: datetime.now(UTC)) -> None:
        self.path = path
        self.clock = clock
        self.seq = 0

    def build(self, bridge: Bridge) -> ops_pb2.StatusSnapshot:
        now = self.clock()
        self.seq += 1
        cfg = bridge.config
        lease = bridge.lease.read()
        initial = bridge.tournament.initial_balance or 100_000.0
        equity = bridge.oms.equity
        snap = ops_pb2.StatusSnapshot(
            header=header("bridge", self.seq, now_ns=int(now.timestamp() * 1e9)),
            mode=bridge.mode,
            stage=stage_for(now, cfg.calendar.lock_utc),
            trader=lease.holder if lease is not None and lease.expires_at_ns > now.timestamp() * 1e9 else "",
            equity=equity,
            cash=cash_of(bridge),
            pnl_total=equity - initial if equity else 0.0,
            field_size=bridge.tournament.member_count or 0,
            killed=bridge.kill.killed(),
            kill_reason=bridge.kill.reason() or "",
        )
        realtime_ok = bridge.realtime is not None and bridge.realtime.connected.is_set()
        fair_age = bridge.market.fair_age_s()
        snap.health.extend(
            [
                ops_pb2.HealthItem(name="platform", state="ok" if bridge.oms.last_reconcile else "degraded"),
                ops_pb2.HealthItem(name="realtime", state="ok" if realtime_ok else "degraded"),
                ops_pb2.HealthItem(
                    name="fair",
                    state="ok" if fair_age is not None and fair_age <= cfg.fair.stale_after_s else "degraded",
                    detail="" if fair_age is None else f"age {fair_age:.0f}s",
                ),
                ops_pb2.HealthItem(name="lease", state="ok" if bridge.lease.held() else "degraded"),
                ops_pb2.HealthItem(
                    name="risk",
                    state="down" if bridge.risk.frozen else "ok",
                    detail=bridge.risk.frozen_reason or "",
                ),
            ]
        )
        marks = bridge.market.marks()
        for ex, qty in sorted(bridge.oms.positions.items()):
            mark = marks.get(ex)
            market_id = bridge.market.exchange_market.get(ex, "")
            title = bridge.market.markets[market_id].title if market_id in bridge.market.markets else ""
            snap.positions.append(
                ops_pb2.PositionSummary(
                    exchange_id=ex,
                    title=title,
                    quantity=qty,
                    mark=(mark / PIPS_PER_UNIT) if mark is not None else 0.0,
                )
            )
        for trade in list(bridge.gateway.recent_trades)[-20:]:
            snap.recent_trades.append(
                ops_pb2.TradeSummary(
                    ts_ns=trade["ts_ns"],
                    strategy=trade["strategy"],
                    title=trade["exchange_id"],
                    side=common_pb2.SIDE_YES if trade["side"] == "yes" else common_pb2.SIDE_NO,
                    action=common_pb2.ACTION_BUY if trade["action"] == "buy" else common_pb2.ACTION_SELL,
                    quantity=trade["quantity"],
                    price=trade["price"],
                )
            )
        if not cfg.has_api_key:
            snap.action_needed.append("Create an API key and put SUSQ_API_KEY in .env")
        snap.action_needed.extend(bridge.gateway.action_needed)
        snap.events.extend(bridge.events[-20:])
        snap.events.extend(
            f"mapping error blocked {ex}: {why}" for ex, why in sorted(bridge.market.flagged.items())
        )
        return snap

    def write(self, bridge: Bridge) -> ops_pb2.StatusSnapshot:
        snap = self.build(bridge)
        atomic_write(self.path, to_json(snap).encode())
        return snap
