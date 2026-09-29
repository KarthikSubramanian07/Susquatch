"""Mirror the dashboard's Kill flag into `ops/KILL`, the file every gateway checks before every write.

Rule: **while the dashboard flag is set, `ops/KILL` exists.** The poller creates the file (with the reason
and who pressed it) when it is missing. It never edits a KILL file a human or the watchdog wrote, and it
**never deletes one**. When the dashboard is unreachable, the poller keeps whatever is on disk and records
the outage so the dashboard and the daily summary show that remote Kill is currently not effective. The
file-based kill (`touch ops/KILL` over SSH) always works.

Un-killing is a deliberate human action, run on the server:

    uv run python -m susq.ops.kill_poller unkill      # or: make unkill

It clears the dashboard flag first (so the poller can't re-create the file), then removes `ops/KILL`.
If the flag can't be cleared, it removes nothing.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from typing import Any

import httpx

from susq.ops.common import (
    ActionItems,
    OpsPaths,
    append_event,
    atomic_write_text,
    load_config,
    ops_setting,
    susq_home,
    utc_now,
)
from susq.ops.status_push import DEFAULT_DASHBOARD_URL

log = logging.getLogger("susq.ops.kill_poller")

UNREACHABLE_ACTION_AFTER_S = 30 * 60


@dataclass(frozen=True)
class KillFlag:
    """The dashboard's `kill` KV record as served by `GET /api/kill`."""

    killed: bool
    reason: str = ""
    at: str = ""
    by: str = ""

    @classmethod
    def parse(cls, data: Any) -> KillFlag:
        """Strict parse: anything that isn't clearly `killed: false` is treated as a malformed response."""
        if not isinstance(data, dict) or not isinstance(data.get("killed"), bool):
            raise ValueError("malformed kill response")
        return cls(
            killed=data["killed"],
            reason=str(data.get("reason", ""))[:500],
            at=str(data.get("at", "")),
            by=str(data.get("by", "")),
        )


class KillPoller:
    def __init__(
        self,
        paths: OpsPaths,
        client: httpx.Client,
        url: str,
        token: str,
        *,
        clock: Callable[[], datetime] = utc_now,
    ) -> None:
        self.paths = paths
        self.client = client
        self.url = url.rstrip("/")
        self.token = token
        self.clock = clock
        self.actions = ActionItems(paths.actions_dir)
        self.first_failure_at: datetime | None = None

    def fetch(self) -> KillFlag:
        resp = self.client.get(f"{self.url}/api/kill", headers={"Authorization": f"Bearer {self.token}"})
        resp.raise_for_status()
        return KillFlag.parse(resp.json())

    def apply(self, flag: KillFlag) -> bool:
        """Make the disk agree with a set flag. Returns True if this call created `ops/KILL`."""
        if not flag.killed or self.paths.kill_file.exists():
            return False
        reason = flag.reason or "dashboard Kill"
        text = (
            f"{reason}\n# set by kill-poller from dashboard flag (by={flag.by or '?'} at={flag.at or '?'})\n"
        )
        atomic_write_text(self.paths.kill_file, text)
        append_event(self.paths, f"KILL applied from dashboard: {reason}", now=self.clock())
        log.warning("ops/KILL written: %s", reason)
        return True

    def _record_ok(self, now: datetime) -> None:
        rec = {"last_ok_utc": now.strftime("%Y-%m-%dT%H:%M:%SZ")}
        atomic_write_text(self.paths.poller_record, json.dumps(rec) + "\n")

    def tick(self) -> KillFlag | None:
        """One poll. Returns the flag, or None when the dashboard couldn't be read."""
        now = self.clock()
        if not self.token:
            self.actions.raise_item(
                "kill-poller-config",
                "Remote Kill is not wired: set SUSQ_STATUS_PUSH_TOKEN in .env on the server.",
            )
            return None
        self.actions.resolve("kill-poller-config")
        try:
            flag = self.fetch()
        except (httpx.HTTPError, ValueError) as exc:
            log.warning("kill poll failed: %s", exc)
            if self.first_failure_at is None:
                self.first_failure_at = now
            down_for = (now - self.first_failure_at).total_seconds()
            if down_for >= UNREACHABLE_ACTION_AFTER_S:
                since = self.first_failure_at.strftime("%Y-%m-%d %H:%M UTC")
                self.actions.raise_item(
                    "kill-poller-unreachable",
                    f"Kill poller cannot read the dashboard since {since}; the dashboard Kill button "
                    "is not reaching the server. SSH `touch ops/KILL` still works.",
                )
            return None
        if self.first_failure_at is not None:
            self.actions.resolve("kill-poller-unreachable")
            self.first_failure_at = None
        self._record_ok(now)
        self.apply(flag)
        return flag

    def unkill(self, *, clear_remote: bool = True) -> None:
        """Human-only: clear the dashboard flag, then remove `ops/KILL`. Raises if the flag stays set."""
        if clear_remote:
            resp = self.client.delete(
                f"{self.url}/api/kill", headers={"Authorization": f"Bearer {self.token}"}
            )
            resp.raise_for_status()
        self.paths.kill_file.unlink(missing_ok=True)
        append_event(self.paths, "KILL cleared by a human (unkill)", now=self.clock())


def _poll_interval(home_config: dict[str, Any]) -> float:
    return float(ops_setting(home_config, "kill_poll_interval_s", 10))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Dashboard Kill -> ops/KILL")
    sub = parser.add_subparsers(dest="cmd")
    sub.add_parser("run", help="poll forever (default)")
    sub.add_parser("once", help="poll once and exit")
    un = sub.add_parser("unkill", help="HUMAN ONLY: clear the dashboard flag and remove ops/KILL")
    un.add_argument("--yes", action="store_true", help="skip the typed confirmation")
    un.add_argument(
        "--local-only",
        action="store_true",
        help="remove ops/KILL without clearing the dashboard flag (stop kill-poller first or it comes back)",
    )
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    logging.getLogger("httpx").setLevel(logging.WARNING)  # one line per poll is noise

    home = susq_home()
    config = load_config(home)
    url = os.environ.get("SUSQ_DASHBOARD_URL", DEFAULT_DASHBOARD_URL)
    token = os.environ.get("SUSQ_STATUS_PUSH_TOKEN", "")
    with httpx.Client(timeout=8.0) as client:
        poller = KillPoller(OpsPaths(home), client, url, token)
        if args.cmd == "unkill":
            if not args.yes:
                typed = input("This re-enables trading. Type 'un-kill' to continue: ").strip()
                if typed != "un-kill":
                    print("aborted; ops/KILL left in place")
                    return 1
            try:
                poller.unkill(clear_remote=not args.local_only)
            except httpx.HTTPError as exc:
                print(f"could not clear the dashboard flag ({exc}); ops/KILL left in place", file=sys.stderr)
                return 2
            print("ops/KILL removed. Gateways resume on their next check (reconcile first, then trade).")
            return 0
        if args.cmd == "once":
            return 0 if poller.tick() is not None else 1
        interval = _poll_interval(config)
        log.info("kill poller -> %s every %ss", url, interval)
        while True:
            try:
                poller.tick()
            except Exception:  # the poller must outlive its own bugs
                log.exception("tick failed")
            time.sleep(interval)


if __name__ == "__main__":
    raise SystemExit(main())
