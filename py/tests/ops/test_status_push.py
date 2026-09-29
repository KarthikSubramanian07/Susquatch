from __future__ import annotations

import json
from datetime import timedelta

import httpx
import pytest
import respx
from susq.contracts import from_json, to_json
from susq.ops.common import ActionItems, OpsPaths, append_event
from susq.ops.status_push import PushConfig, StatusPusher
from susq.v1 import ops_pb2

from .conftest import FIXTURES, Clock

URL = "https://dash.test"


def cfg(**kw: object) -> PushConfig:
    base: dict[str, object] = {
        "url": URL,
        "token": "push-token-for-tests",
        "interval_s": 150,
        "daily_budget": 5,
    }
    base.update(kw)
    return PushConfig(**base)  # type: ignore[arg-type]


def write_status(paths: OpsPaths, clock: Clock, *, seq: int = 1, events: list[str] | None = None) -> None:
    snap = from_json(ops_pb2.StatusSnapshot, (FIXTURES / "status_snapshot.json").read_text())
    snap.header.seq = seq
    snap.header.produced_at_ns = int(clock().timestamp() * 1e9)
    del snap.events[:]
    snap.events.extend(events or [])
    paths.status_file.write_text(to_json(snap))


def pushed(route: respx.Route, i: int = -1) -> ops_pb2.StatusSnapshot:
    return from_json(ops_pb2.StatusSnapshot, route.calls[i].request.content.decode())


@pytest.fixture
def api() -> respx.MockRouter:
    with respx.mock(base_url=URL, assert_all_called=False) as router:
        yield router


def make(paths: OpsPaths, clock: Clock, **kw: object) -> StatusPusher:
    return StatusPusher(paths, cfg(**kw), httpx.Client(), clock=clock)


def test_pushes_writer_snapshot_with_bearer_and_ops_overlay(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200, json={"ok": True})
    write_status(paths, clock, events=["first qualifying trade filled"])
    p = make(paths, clock)

    assert p.tick() is True
    req = route.calls[0].request
    assert req.headers["authorization"] == "Bearer push-token-for-tests"
    snap = pushed(route)
    assert snap.equity == pytest.approx(100431.5)
    assert snap.header.producer.name == "status-pusher"
    names = {h.name: h.state for h in snap.health}
    assert names["platform"] == "ok"
    assert names["status-writer"] == "ok"
    assert names["backup"] == "degraded"  # no backup yet
    assert any(e.endswith("first qualifying trade filled") for e in snap.events)


def test_missing_status_file_still_pushes_a_heartbeat(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    assert make(paths, clock).tick() is True
    snap = pushed(route)
    assert snap.trader == "none"
    assert {h.name: h.state for h in snap.health}["status-writer"] == "down"


def test_stale_writer_is_reported_down(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    write_status(paths, clock)
    clock.advance(900)
    make(paths, clock).tick()
    writer = next(h for h in pushed(route).health if h.name == "status-writer")
    assert writer.state == "down"
    assert "15m" in writer.detail


def test_kill_file_on_disk_wins_over_writer(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    write_status(paths, clock)
    paths.kill_file.write_text("human pressed Kill\n")
    make(paths, clock).tick()
    snap = pushed(route)
    assert snap.killed is True
    assert snap.kill_reason == "human pressed Kill"


def test_action_items_are_merged_once(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    ActionItems(paths.actions_dir).raise_item("r2-quota", "R2 at 82% of free tier: backups paused.")
    make(paths, clock).tick()
    assert list(pushed(route).action_needed) == ["R2 at 82% of free tier: backups paused."]


def test_events_survive_a_failed_push_and_are_not_duplicated(paths, clock, api) -> None:
    route = api.post("/api/push")
    route.side_effect = [httpx.Response(503), httpx.Response(200), httpx.Response(200)]
    write_status(paths, clock, seq=1, events=["breaker tripped"])
    append_event(paths, "backup uploaded", now=clock())
    p = make(paths, clock)

    assert p.tick() is False
    clock.advance(p.cfg.interval_s)
    assert p.tick() is True  # same seq: event collected once, retried
    events = list(pushed(route, 1).events)
    assert sum("breaker tripped" in e for e in events) == 1
    assert sum("backup uploaded" in e for e in events) == 1

    clock.advance(p.cfg.interval_s)
    assert p.tick() is True
    assert list(pushed(route, 2).events) == []


def test_network_error_backs_off_without_raising(paths, clock, api) -> None:
    api.post("/api/push").mock(side_effect=httpx.ConnectError("down"))
    p = make(paths, clock)
    assert p.tick() is False
    assert p.tick() is None  # inside the backoff window, nothing attempted
    clock.advance(31)
    assert p.tick() is False


def test_not_due_until_interval_passes(paths, clock, api) -> None:
    api.post("/api/push").respond(200)
    p = make(paths, clock)
    assert p.tick() is True
    clock.advance(60)
    assert p.tick() is None
    clock.advance(91)
    assert p.tick() is True


def test_daily_budget_stops_pushes_and_files_action(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    p = make(paths, clock, daily_budget=2)
    for _ in range(3):
        p.tick()
        clock.advance(200)
    assert route.call_count == 2
    assert any("daily budget" in i for i in ActionItems(paths.actions_dir).items())
    record = json.loads(paths.push_record.read_text())
    assert record["count"] == 2


def test_missing_token_files_action_and_sends_nothing(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    p = make(paths, clock, token="")
    assert p.tick() is None
    assert route.call_count == 0
    assert any("SUSQ_STATUS_PUSH_TOKEN" in i for i in ActionItems(paths.actions_dir).items())


def test_election_window_pushes_faster(clock) -> None:
    c = PushConfig.load(
        {
            "ops": {
                "status_push_interval_s": 150,
                "status_push_interval_election_s": 120,
                "priority_window_start_utc": "2026-11-03T23:00:00Z",
                "priority_window_end_utc": "2026-11-04T17:00:00Z",
            }
        },
        {"SUSQ_STATUS_PUSH_TOKEN": "x"},
    )
    night = c.window_start
    assert night is not None
    assert c.interval_at(night + timedelta(hours=3)) == 120
    assert c.interval_at(night - timedelta(seconds=1)) == 150
    assert c.url == "https://susquatch.pages.dev"


def test_backup_record_feeds_health(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    last = (clock() - timedelta(hours=40)).strftime("%Y-%m-%dT%H:%M:%SZ")
    paths.backup_record.write_text(json.dumps({"last_success_utc": last}))
    make(paths, clock).tick()
    backup = next(h for h in pushed(route).health if h.name == "backup")
    assert backup.state == "degraded"
    assert "40h" in backup.detail


def test_research_scheduler_health(paths, clock, api) -> None:
    route = api.post("/api/push").respond(200)
    p = make(paths, clock)
    p.tick()
    assert next(h for h in pushed(route).health if h.name == "research").state == "down"

    paths.scheduler_health.parent.mkdir(parents=True, exist_ok=True)
    doc = {
        "written_utc": clock().isoformat(),
        "jobs": {"collect": {"consecutive_failures": 0}, "fair": {"consecutive_failures": 4}},
    }
    paths.scheduler_health.write_text(json.dumps(doc))
    clock.advance(200)
    doc["written_utc"] = clock().isoformat()
    paths.scheduler_health.write_text(json.dumps(doc))
    p.tick()
    research = next(h for h in pushed(route).health if h.name == "research")
    assert research.state == "degraded"
    assert research.detail == "failing: fair"

    clock.advance(400)  # the file is now 400 s old: hung
    p.tick()
    assert next(h for h in pushed(route).health if h.name == "research").state == "down"


def test_sliding_window_events_are_pushed_once(paths, clock, api) -> None:
    """The bridge keeps its last 20 events in every snapshot; each must reach the dashboard once."""
    route = api.post("/api/push").respond(200)
    p = make(paths, clock)
    write_status(paths, clock, seq=1, events=["lease acquired", "mapping error blocked 381: inverted"])
    p.tick()
    clock.advance(p.cfg.interval_s)
    write_status(
        paths, clock, seq=2, events=["lease acquired", "mapping error blocked 381: inverted", "racer filled"]
    )
    p.tick()
    clock.advance(p.cfg.interval_s)
    write_status(
        paths, clock, seq=3, events=["mapping error blocked 381: inverted", "racer filled", "racer filled"]
    )
    p.tick()
    texts = [e.split(" ", 1)[1] for i in range(3) for e in pushed(route, i).events]
    assert texts == ["lease acquired", "mapping error blocked 381: inverted", "racer filled", "racer filled"]
