"""The watchdog: restarts unhealthy containers and enforces the post-lock kill. It never alerts anyone.

Compose's `restart: unless-stopped` already revives processes that *exit*. The watchdog covers the other case:
a process that is still running but whose healthcheck fails (hung event loop, dead websocket). It restarts
it, at most `MAX_RESTARTS_PER_HOUR` times per service. Past that it stops restarting and files an ACTION
NEEDED item, because a crash loop that hammers the exchange on every reconnect is worse than a quiet
degrade.

It also enforces the calendar. At or after `calendar.last_order_utc` (Nov 4 16:55Z) it makes sure `ops/KILL`
exists, so every gateway cancels all orders and freezes, whatever state the strategies are in. This backs up
the gateways' own time gate and the GTD expiry on every order.

Docker is reached through a socket proxy that only permits listing and restarting containers
(see ops/compose.yaml), never the raw socket.
"""

from __future__ import annotations

import json
import logging
import os
import time
from collections import defaultdict, deque
from collections.abc import Callable
from datetime import datetime, timedelta
from typing import Any

import httpx

from susq.ops.common import (
    ActionItems,
    OpsPaths,
    append_event,
    atomic_write_text,
    load_config,
    parse_utc,
    susq_home,
    utc_now,
)

log = logging.getLogger("susq.ops.watchdog")

MAX_RESTARTS_PER_HOUR = 3
TICK_S = 30


class Watchdog:
    def __init__(
        self,
        paths: OpsPaths,
        docker: httpx.Client,
        *,
        project: str,
        last_order_utc: datetime | None,
        clock: Callable[[], datetime] = utc_now,
    ) -> None:
        self.paths = paths
        self.docker = docker
        self.project = project
        self.last_order_utc = last_order_utc
        self.clock = clock
        self.actions = ActionItems(paths.actions_dir)
        self.restarts: dict[str, deque[datetime]] = defaultdict(deque)

    def unhealthy(self) -> list[dict[str, Any]]:
        filters = {"label": [f"com.docker.compose.project={self.project}"], "health": ["unhealthy"]}
        resp = self.docker.get("/containers/json", params={"filters": json.dumps(filters)})
        resp.raise_for_status()
        data = resp.json()
        return data if isinstance(data, list) else []

    def _service(self, container: dict[str, Any]) -> str:
        labels = container.get("Labels") or {}
        return str(
            labels.get("com.docker.compose.service") or (container.get("Names") or ["?"])[0].lstrip("/")
        )

    def heal(self) -> list[str]:
        """Restart unhealthy containers within the per-service budget. Returns the services restarted."""
        now = self.clock()
        restarted = []
        sick = [c for c in self.unhealthy() if self._service(c) != "watchdog"]
        self._resolve_recovered({self._service(c) for c in sick})
        for c in sick:
            service = self._service(c)
            history = self.restarts[service]
            while history and now - history[0] > timedelta(hours=1):
                history.popleft()
            if len(history) >= MAX_RESTARTS_PER_HOUR:
                self.actions.raise_item(
                    f"watchdog-{service}",
                    f"{service} stayed unhealthy after {MAX_RESTARTS_PER_HOUR} restarts in an hour; "
                    "the watchdog stopped restarting it. Check `make logs`.",
                )
                continue
            resp = self.docker.post(f"/containers/{c['Id']}/restart", params={"t": 20})
            if resp.is_success:
                history.append(now)
                restarted.append(service)
                append_event(self.paths, f"watchdog restarted unhealthy {service}", now=now)
                log.warning("restarted unhealthy %s", service)
            else:
                log.error("restart of %s failed: %s", service, resp.status_code)
        return restarted

    def _resolve_recovered(self, unhealthy_services: set[str]) -> None:
        """Clear crash-loop items for services that are healthy again."""
        for service in list(self.restarts):
            if service not in unhealthy_services:
                self.actions.resolve(f"watchdog-{service}")

    def enforce_lock(self) -> bool:
        """At/after the last order time, `ops/KILL` must exist. Returns True if this call created it."""
        if self.last_order_utc is None or self.clock() < self.last_order_utc:
            return False
        if self.paths.kill_file.exists():
            return False
        stamp = self.last_order_utc.strftime("%Y-%m-%dT%H:%MZ")
        atomic_write_text(
            self.paths.kill_file, f"watchdog: post-lock time gate ({stamp}); no orders after this\n"
        )
        append_event(self.paths, "watchdog wrote ops/KILL at the post-lock time gate", now=self.clock())
        return True

    def beat(self) -> None:
        stamp = self.clock().strftime("%Y-%m-%dT%H:%M:%SZ")
        atomic_write_text(self.paths.ops_state / "watchdog.json", json.dumps({"last_tick_utc": stamp}) + "\n")

    def tick(self) -> None:
        self.enforce_lock()
        try:
            self.heal()
        except httpx.HTTPError as exc:
            log.warning("docker api unavailable: %s", exc)
        self.beat()


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    home = susq_home()
    config = load_config(home)
    last = config.get("calendar", {}).get("last_order_utc")
    docker_url = os.environ.get("SUSQ_DOCKER_API", "http://docker-proxy:2375")
    project = os.environ.get("COMPOSE_PROJECT_NAME", "susquatch")
    with httpx.Client(base_url=docker_url, timeout=30.0) as docker:
        dog = Watchdog(
            OpsPaths(home), docker, project=project, last_order_utc=parse_utc(last) if last else None
        )
        log.info("watchdog up: project=%s docker=%s", project, docker_url)
        while True:
            try:
                dog.tick()
            except Exception:  # the watchdog must outlive its own bugs
                log.exception("tick failed")
            time.sleep(TICK_S)


if __name__ == "__main__":
    raise SystemExit(main())
