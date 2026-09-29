from __future__ import annotations

import json
from datetime import timedelta

import httpx
import pytest
import respx
from susq.ops.common import ActionItems, drain_events
from susq.ops.watchdog import MAX_RESTARTS_PER_HOUR, Watchdog

from .conftest import T0

DOCKER = "http://docker.test"


def container(cid: str, service: str) -> dict[str, object]:
    return {
        "Id": cid,
        "Names": [f"/susquatch-{service}-1"],
        "Labels": {"com.docker.compose.service": service},
    }


@pytest.fixture
def api() -> respx.MockRouter:
    with respx.mock(base_url=DOCKER, assert_all_called=False) as router:
        yield router


def make(paths, clock, last_order=T0 + timedelta(days=30)) -> Watchdog:
    return Watchdog(
        paths, httpx.Client(base_url=DOCKER), project="susquatch", last_order_utc=last_order, clock=clock
    )


def test_restarts_unhealthy_but_never_itself(paths, clock, api) -> None:
    listing = api.get("/containers/json").respond(
        200, json=[container("abc", "needle"), container("zzz", "watchdog")]
    )
    restart = api.post("/containers/abc/restart").respond(204)
    self_restart = api.post("/containers/zzz/restart").respond(204)
    assert make(paths, clock).heal() == ["needle"]
    assert restart.call_count == 1
    assert self_restart.call_count == 0
    filters = json.loads(listing.calls[0].request.url.params["filters"])
    assert filters == {"label": ["com.docker.compose.project=susquatch"], "health": ["unhealthy"]}
    assert any("restarted unhealthy needle" in e for e in drain_events(paths))


def test_restart_budget_then_action_item_then_recovery(paths, clock, api) -> None:
    listing = api.get("/containers/json").respond(200, json=[container("abc", "rust-live")])
    restart = api.post("/containers/abc/restart").respond(204)
    dog = make(paths, clock)
    for _ in range(MAX_RESTARTS_PER_HOUR + 2):
        dog.tick()
        clock.advance(60)
    assert restart.call_count == MAX_RESTARTS_PER_HOUR
    assert any("rust-live stayed unhealthy" in i for i in ActionItems(paths.actions_dir).items())

    listing.respond(200, json=[])
    dog.tick()
    assert ActionItems(paths.actions_dir).items() == []

    clock.advance(3600)
    listing.respond(200, json=[container("abc", "rust-live")])
    dog.tick()
    assert restart.call_count == MAX_RESTARTS_PER_HOUR + 1  # budget window rolled over


def test_docker_outage_does_not_raise_and_still_beats(paths, clock, api) -> None:
    api.get("/containers/json").mock(side_effect=httpx.ConnectError("no proxy"))
    make(paths, clock).tick()
    assert json.loads((paths.ops_state / "watchdog.json").read_text())["last_tick_utc"]


def test_post_lock_kill(paths, clock, api) -> None:
    api.get("/containers/json").respond(200, json=[])
    dog = make(paths, clock, last_order=T0 + timedelta(minutes=5))
    dog.tick()
    assert not paths.kill_file.exists()
    clock.advance(300)
    dog.tick()
    assert "post-lock time gate" in paths.kill_file.read_text()


def test_post_lock_keeps_existing_human_reason(paths, clock) -> None:
    paths.kill_file.write_text("human: done for the night\n")
    dog = make(paths, clock, last_order=T0)
    assert dog.enforce_lock() is False
    assert paths.kill_file.read_text() == "human: done for the night\n"
