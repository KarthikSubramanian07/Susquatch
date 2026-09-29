"""Ship `state/status.json` to the dashboard (`POST /api/push`) every few minutes.

The lease holder (bridge or rust-live) writes a `StatusSnapshot` to `state/status.json`. This process:

1. reads it every `poll_s` seconds, collecting each new snapshot's `events` so none are lost between pushes;
2. overlays what only ops knows: open ACTION NEEDED items, whether `ops/KILL` exists on disk, whether the
   status writer has gone quiet, and backup / self-check / kill-poller health;
3. pushes on a cadence sized to Cloudflare KV's free tier (1,000 writes/day, shared), with a hard daily
   budget, and never crashes on an outage. Failed pushes keep their events for the next attempt.

If the whole server dies, the pushes stop. The dashboard and the daily Worker then show "server silent since
…". That dead-man signal is the point: silence is visible without an alert.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import time
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Any

import httpx
from google.protobuf import json_format

from susq.contracts import from_json, header
from susq.ops.common import (
    ActionItems,
    OpsPaths,
    atomic_write_text,
    drain_events,
    load_config,
    ops_setting,
    parse_utc,
    susq_home,
    utc_now,
)
from susq.v1 import ops_pb2

log = logging.getLogger("susq.ops.status_push")

DEFAULT_DASHBOARD_URL = "https://susquatch.pages.dev"
MAX_PENDING_EVENTS = 200


@dataclass(frozen=True)
class PushConfig:
    """Knobs for the pusher. Defaults mirror `[ops]` in config/susq.toml."""

    url: str
    token: str
    interval_s: float = 150
    election_interval_s: float = 120
    window_start: datetime | None = None
    window_end: datetime | None = None
    daily_budget: int = 800
    stale_after_s: float = 300
    backup_max_age_h: float = 30
    poll_s: float = 15

    @classmethod
    def load(cls, config: dict[str, Any], env: dict[str, str]) -> PushConfig:
        start = ops_setting(config, "priority_window_start_utc", None)
        end = ops_setting(config, "priority_window_end_utc", None)
        return cls(
            url=env.get("SUSQ_DASHBOARD_URL", DEFAULT_DASHBOARD_URL).rstrip("/"),
            token=env.get("SUSQ_STATUS_PUSH_TOKEN", ""),
            interval_s=float(ops_setting(config, "status_push_interval_s", 150)),
            election_interval_s=float(ops_setting(config, "status_push_interval_election_s", 120)),
            window_start=parse_utc(start) if start else None,
            window_end=parse_utc(end) if end else None,
            daily_budget=int(ops_setting(config, "status_push_daily_budget", 800)),
            stale_after_s=float(ops_setting(config, "status_stale_after_s", 300)),
            backup_max_age_h=float(ops_setting(config, "backup_max_age_h", 30)),
        )

    def interval_at(self, now: datetime) -> float:
        """Push a little faster during the election-night window (still inside the KV budget)."""
        if self.window_start and self.window_end and self.window_start <= now < self.window_end:
            return self.election_interval_s
        return self.interval_s


def _age_text(seconds: float) -> str:
    seconds = max(0, int(seconds))
    if seconds < 120:
        return f"{seconds}s"
    if seconds < 7200:
        return f"{seconds // 60}m"
    return f"{seconds // 3600}h{(seconds % 3600) // 60:02d}m"


def _read_json(path: os.PathLike[str]) -> dict[str, Any] | None:
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


class StatusPusher:
    """One pusher instance: `tick()` is called in a loop; all state lives here or in `ops/state/`."""

    def __init__(
        self,
        paths: OpsPaths,
        cfg: PushConfig,
        client: httpx.Client,
        *,
        clock: Callable[[], datetime] = utc_now,
    ) -> None:
        self.paths = paths
        self.cfg = cfg
        self.client = client
        self.clock = clock
        self.actions = ActionItems(paths.actions_dir)
        self.pending_events: deque[str] = deque(maxlen=MAX_PENDING_EVENTS)
        self.last_seen_seq: int | None = None
        self.last_push_at: datetime | None = None
        self.next_attempt_at: datetime | None = None
        self.failures = 0
        self.seq = 0

    # ── reading ────────────────────────────────────────────────────────────
    def read_snapshot(self) -> ops_pb2.StatusSnapshot | None:
        """Parse `state/status.json`; queue its events the first time we see its `seq`."""
        try:
            text = self.paths.status_file.read_text(encoding="utf-8")
            snap = from_json(ops_pb2.StatusSnapshot, text)
        except (OSError, json_format.ParseError) as exc:
            log.debug("status.json unreadable: %s", exc)
            return None
        seq = snap.header.seq
        if seq != self.last_seen_seq:
            self.last_seen_seq = seq
            stamp = self.clock().strftime("%Y-%m-%dT%H:%M:%SZ")
            self.pending_events.extend(f"{stamp} {e}" for e in snap.events)
        return snap

    # ── building ───────────────────────────────────────────────────────────
    def build(self, snap: ops_pb2.StatusSnapshot | None) -> ops_pb2.StatusSnapshot:
        """The snapshot to push: the writer's view plus everything ops knows that it might not."""
        now = self.clock()
        now_ns = int(now.timestamp() * 1e9)
        out = ops_pb2.StatusSnapshot()
        if snap is not None:
            out.CopyFrom(snap)
        else:
            out.mode = os.environ.get("SUSQ_MODE", "paper")
            out.trader = "none"
        self.seq += 1
        # The pusher's header proves the pusher is alive; the writer's age is reported as health below.
        writer_ns = snap.header.produced_at_ns if snap is not None else 0
        out.header.CopyFrom(header("status-pusher", self.seq, now_ns=now_ns))

        # Kill truth is the file on disk, whatever the (possibly hung) writer last said.
        if self.paths.kill_file.exists():
            out.killed = True
            try:
                reason = self.paths.kill_file.read_text(encoding="utf-8").strip()
            except OSError:
                reason = ""
            out.kill_reason = (reason or "ops/KILL present")[:500]

        del out.health[:]
        if snap is not None:
            out.health.extend(snap.health)
        out.health.extend(self._ops_health(now, writer_ns))

        existing = list(out.action_needed)
        for item in self.actions.items():
            if item not in existing:
                existing.append(item)
        del out.action_needed[:]
        out.action_needed.extend(existing)

        self.pending_events.extend(drain_events(self.paths))
        del out.events[:]
        out.events.extend(self.pending_events)
        return out

    def _ops_health(self, now: datetime, writer_ns: int) -> list[ops_pb2.HealthItem]:
        items: list[ops_pb2.HealthItem] = []
        now_ns = int(now.timestamp() * 1e9)

        if writer_ns == 0:
            items.append(
                ops_pb2.HealthItem(name="status-writer", state="down", detail="no state/status.json")
            )
        else:
            age = (now_ns - writer_ns) / 1e9
            state = "ok" if age <= self.cfg.stale_after_s else "down"
            detail = "" if state == "ok" else f"state/status.json not updated for {_age_text(age)}"
            items.append(
                ops_pb2.HealthItem(name="status-writer", state=state, detail=detail, last_ok_ns=writer_ns)
            )

        backup = _read_json(self.paths.backup_record)
        if backup and backup.get("last_success_utc"):
            last = parse_utc(str(backup["last_success_utc"]))
            age = (now - last).total_seconds()
            ok = age <= self.cfg.backup_max_age_h * 3600
            items.append(
                ops_pb2.HealthItem(
                    name="backup",
                    state="ok" if ok else "degraded",
                    detail=f"last good backup {_age_text(age)} ago",
                    last_ok_ns=int(last.timestamp() * 1e9),
                )
            )
        else:
            items.append(
                ops_pb2.HealthItem(name="backup", state="degraded", detail="no successful backup yet")
            )

        check = _read_json(self.paths.selfcheck_record)
        if check:
            items.append(
                ops_pb2.HealthItem(
                    name="selfcheck",
                    state=str(check.get("state", "degraded")),
                    detail=str(check.get("summary", ""))[:200],
                )
            )

        items.append(self._research_health(now))

        poller = _read_json(self.paths.poller_record)
        if poller and poller.get("last_ok_utc"):
            last = parse_utc(str(poller["last_ok_utc"]))
            age = (now - last).total_seconds()
            items.append(
                ops_pb2.HealthItem(
                    name="kill-poller",
                    state="ok" if age < 120 else "degraded",
                    detail="" if age < 120 else f"cannot reach dashboard for {_age_text(age)}",
                    last_ok_ns=int(last.timestamp() * 1e9),
                )
            )
        return items

    def _research_health(self, now: datetime) -> ops_pb2.HealthItem:
        """The research scheduler's own job health (state/research/scheduler.json)."""
        doc = _read_json(self.paths.scheduler_health)
        if not doc or not doc.get("written_utc"):
            return ops_pb2.HealthItem(name="research", state="down", detail="scheduler not running")
        written = datetime.fromisoformat(str(doc["written_utc"]).replace("Z", "+00:00"))
        age = (now - written).total_seconds()
        written_ns = int(written.timestamp() * 1e9)
        if age > self.cfg.stale_after_s:
            return ops_pb2.HealthItem(
                name="research",
                state="down",
                detail=f"no job has run for {_age_text(age)}",
                last_ok_ns=written_ns,
            )
        jobs = doc.get("jobs") or {}
        failing = sorted(k for k, v in jobs.items() if int((v or {}).get("consecutive_failures", 0)) >= 3)
        if failing:
            return ops_pb2.HealthItem(
                name="research",
                state="degraded",
                detail="failing: " + ", ".join(failing),
                last_ok_ns=written_ns,
            )
        return ops_pb2.HealthItem(
            name="research", state="ok", detail=f"{len(jobs)} jobs ok", last_ok_ns=written_ns
        )

    # ── pushing ────────────────────────────────────────────────────────────
    def _budget_used(self, today: str) -> int:
        rec = _read_json(self.paths.push_record) or {}
        return int(rec.get("count", 0)) if rec.get("date") == today else 0

    def _record_push(self, today: str, used: int, now: datetime) -> None:
        rec = {"date": today, "count": used, "last_ok_utc": now.strftime("%Y-%m-%dT%H:%M:%SZ")}
        atomic_write_text(self.paths.push_record, json.dumps(rec) + "\n")

    def push(self, snap: ops_pb2.StatusSnapshot) -> bool:
        """POST one snapshot. True only on 2xx; never raises on network trouble."""
        body = json_format.MessageToJson(snap, indent=None)
        try:
            resp = self.client.post(
                f"{self.cfg.url}/api/push",
                content=body,
                headers={"Authorization": f"Bearer {self.cfg.token}", "Content-Type": "application/json"},
            )
        except httpx.HTTPError as exc:
            log.warning("push failed: %s", exc)
            return False
        if resp.is_success:
            return True
        log.warning("push rejected: %s %s", resp.status_code, resp.text[:200])
        return False

    def due(self, now: datetime) -> bool:
        if self.next_attempt_at is not None and now < self.next_attempt_at:
            return False
        if self.last_push_at is None:
            return True
        return (now - self.last_push_at).total_seconds() >= self.cfg.interval_at(now)

    def tick(self) -> bool | None:
        """Read the snapshot; push if due. Returns True/False for a push attempt, None if nothing was due."""
        snap = self.read_snapshot()
        now = self.clock()
        if not self.cfg.token:
            self.actions.raise_item(
                "status-push-config",
                "Status push is not configured: set SUSQ_STATUS_PUSH_TOKEN in .env (docs/OPS.md, Secrets).",
            )
            return None
        self.actions.resolve("status-push-config")
        if not self.due(now):
            return None

        today = now.strftime("%Y-%m-%d")
        used = self._budget_used(today)
        if used >= self.cfg.daily_budget:
            self.actions.raise_item(
                "status-push-budget",
                f"Status push hit its daily budget ({self.cfg.daily_budget} KV writes) on {today}; "
                "the dashboard is frozen until 00:00 UTC. Check for a push loop.",
            )
            self.next_attempt_at = (now + timedelta(days=1)).replace(hour=0, minute=0, second=5)
            return None
        self.actions.resolve("status-push-budget")

        payload = self.build(snap)
        ok = self.push(payload)
        if ok:
            self.pending_events.clear()
            self.last_push_at = now
            self.next_attempt_at = None
            self.failures = 0
            self._record_push(today, used + 1, now)
        else:
            self.failures += 1
            backoff = min(self.cfg.interval_at(now), 15 * 2 ** min(self.failures, 6))
            self.next_attempt_at = now + timedelta(seconds=backoff)
        return ok


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--once", action="store_true", help="build and push one snapshot, then exit")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    logging.getLogger("httpx").setLevel(logging.WARNING)  # one line per poll is noise

    home = susq_home()
    cfg = PushConfig.load(load_config(home), dict(os.environ))
    with httpx.Client(timeout=15.0) as client:
        pusher = StatusPusher(OpsPaths(home), cfg, client)
        if args.once:
            pusher.last_push_at = None
            return 0 if pusher.tick() else 1
        log.info("status pusher -> %s every %ss", cfg.url, cfg.interval_s)
        while True:
            try:
                pusher.tick()
            except Exception:  # a pusher bug must never kill the pusher
                log.exception("tick failed")
            time.sleep(cfg.poll_s)


if __name__ == "__main__":
    raise SystemExit(main())
