from __future__ import annotations

import json

import httpx
import pytest
import respx
from susq.ops.common import ActionItems, drain_events
from susq.ops.kill_poller import KillFlag, KillPoller, main

URL = "https://dash.test"


@pytest.fixture
def api() -> respx.MockRouter:
    with respx.mock(base_url=URL, assert_all_called=False) as router:
        yield router


def make(paths, clock, token: str = "push-token-for-tests") -> KillPoller:
    return KillPoller(paths, httpx.Client(), URL, token, clock=clock)


def test_set_flag_writes_kill_file_with_reason(paths, clock, api) -> None:
    route = api.get("/api/kill").respond(
        200,
        json={
            "killed": True,
            "reason": "fat finger on MI-SEN",
            "at": "2026-11-04T02:10:00Z",
            "by": "dashboard",
        },
    )
    p = make(paths, clock)
    assert p.tick() == KillFlag(True, "fat finger on MI-SEN", "2026-11-04T02:10:00Z", "dashboard")
    assert route.calls[0].request.headers["authorization"] == "Bearer push-token-for-tests"
    assert paths.kill_file.read_text().startswith("fat finger on MI-SEN\n")
    assert any("KILL applied" in e for e in drain_events(paths))


def test_existing_kill_file_is_never_overwritten(paths, clock, api) -> None:
    api.get("/api/kill").respond(200, json={"killed": True, "reason": "dashboard"})
    paths.kill_file.write_text("human over ssh: stop everything\n")
    make(paths, clock).tick()
    assert paths.kill_file.read_text() == "human over ssh: stop everything\n"


def test_cleared_flag_never_deletes_kill_file(paths, clock, api) -> None:
    api.get("/api/kill").respond(200, json={"killed": False})
    paths.kill_file.write_text("watchdog: post-lock\n")
    make(paths, clock).tick()
    assert paths.kill_file.exists()


def test_flag_recreates_file_a_human_deleted_without_unkill(paths, clock, api) -> None:
    api.get("/api/kill").respond(200, json={"killed": True, "reason": "dashboard"})
    p = make(paths, clock)
    p.tick()
    paths.kill_file.unlink()
    p.tick()
    assert paths.kill_file.exists()  # fail safe: the flag is still set


@pytest.mark.parametrize(
    "body",
    [{"killed": "no"}, {"reason": "x"}, ["killed"], "garbage"],
)
def test_malformed_response_changes_nothing(paths, clock, api, body) -> None:
    api.get("/api/kill").respond(200, json=body)
    assert make(paths, clock).tick() is None
    assert not paths.kill_file.exists()


def test_outage_is_reported_after_thirty_minutes_then_resolved(paths, clock, api) -> None:
    route = api.get("/api/kill")
    route.side_effect = httpx.ConnectError("no route")
    p = make(paths, clock)
    p.tick()
    clock.advance(29 * 60)
    p.tick()
    assert ActionItems(paths.actions_dir).items() == []
    clock.advance(2 * 60)
    p.tick()
    assert any("cannot read the dashboard" in i for i in ActionItems(paths.actions_dir).items())

    route.side_effect = None
    route.return_value = httpx.Response(200, json={"killed": False})
    assert p.tick() == KillFlag(False)
    assert ActionItems(paths.actions_dir).items() == []
    assert json.loads(paths.poller_record.read_text())["last_ok_utc"]


def test_server_error_is_an_outage_not_a_flag(paths, clock, api) -> None:
    api.get("/api/kill").respond(500)
    assert make(paths, clock).tick() is None


def test_missing_token_files_action(paths, clock, api) -> None:
    route = api.get("/api/kill").respond(200, json={"killed": False})
    assert make(paths, clock, token="").tick() is None
    assert route.call_count == 0
    assert ActionItems(paths.actions_dir).items()


def test_unkill_clears_remote_before_local(paths, clock, api) -> None:
    route = api.delete("/api/kill").respond(200, json={"killed": False})
    paths.kill_file.write_text("dashboard\n")
    make(paths, clock).unkill()
    assert route.call_count == 1
    assert not paths.kill_file.exists()


def test_unkill_keeps_file_when_remote_clear_fails(paths, clock, api) -> None:
    api.delete("/api/kill").respond(503)
    paths.kill_file.write_text("dashboard\n")
    with pytest.raises(httpx.HTTPStatusError):
        make(paths, clock).unkill()
    assert paths.kill_file.exists()


def test_unkill_cli_requires_typed_confirmation(paths, home, monkeypatch, api) -> None:
    monkeypatch.setenv("SUSQ_HOME", str(home))
    monkeypatch.setenv("SUSQ_DASHBOARD_URL", URL)
    monkeypatch.setenv("SUSQ_STATUS_PUSH_TOKEN", "t")
    monkeypatch.setattr("builtins.input", lambda _prompt: "yes")
    paths.kill_file.write_text("x\n")
    assert main(["unkill"]) == 1
    assert paths.kill_file.exists()
