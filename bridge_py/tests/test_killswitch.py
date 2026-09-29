"""KillSwitch and resolve_mode: kill reasons and the two-key live arming."""

from __future__ import annotations

from pathlib import Path

import pytest
from susq_bridge.killswitch import KillSwitch, resolve_mode


@pytest.fixture
def ks(tmp_path: Path) -> KillSwitch:
    return KillSwitch(tmp_path / "ops" / "KILL", tmp_path / "ops" / "GO_LIVE")


def arm(tmp_path: Path, text: str) -> None:
    (tmp_path / "ops").mkdir(exist_ok=True)
    (tmp_path / "ops" / "GO_LIVE").write_text(text)


def test_not_killed_without_file(ks: KillSwitch) -> None:
    assert ks.reason() is None
    assert not ks.killed()


def test_kill_reason_text(ks: KillSwitch, tmp_path: Path) -> None:
    (tmp_path / "ops").mkdir()
    (tmp_path / "ops" / "KILL").write_text("  loss breaker tripped \n")
    assert ks.reason() == "loss breaker tripped"
    assert ks.killed()


def test_empty_kill_file_is_killed(ks: KillSwitch, tmp_path: Path) -> None:
    (tmp_path / "ops").mkdir()
    (tmp_path / "ops" / "KILL").write_text("  \n")
    assert ks.reason() == "killed"


def test_unreadable_kill_file_is_killed(ks: KillSwitch, tmp_path: Path) -> None:
    (tmp_path / "ops" / "KILL").mkdir(parents=True)
    assert ks.reason() == "killed"


def test_trigger_creates_parents_atomically(ks: KillSwitch, tmp_path: Path) -> None:
    ks.trigger("watchdog: needle down")
    assert ks.reason() == "watchdog: needle down"
    assert [p.name for p in (tmp_path / "ops").iterdir()] == ["KILL"]


@pytest.mark.parametrize(
    ("text", "armed"),
    [
        ("go live", True),
        ("Go   LIVE", True),
        ("I say:\n  GO\tlive  now\n", True),
        ("go", False),
        ("golive", False),
        ("", False),
    ],
)
def test_go_live_phrase(ks: KillSwitch, tmp_path: Path, text: str, armed: bool) -> None:
    arm(tmp_path, text)
    assert ks.go_live_armed() is armed


def test_go_live_missing_file(ks: KillSwitch) -> None:
    assert not ks.go_live_armed()


@pytest.mark.parametrize(
    ("default", "env", "go_live", "expected"),
    [
        ("paper", {"SUSQ_MODE": "live"}, "go live", "live"),
        ("paper", {"SUSQ_MODE": " LIVE "}, "Go   LIVE", "live"),
        ("paper", {"SUSQ_MODE": "live"}, None, "paper"),
        ("paper", {"SUSQ_MODE": "live"}, "go", "paper"),
        ("paper", {"SUSQ_MODE": "paper"}, "go live", "paper"),
        ("live", {"SUSQ_MODE": "paper"}, "go live", "paper"),
        ("live", {}, "go live", "live"),
        ("live", {}, None, "paper"),
        ("paper", {}, "go live", "paper"),
        ("paper", {"SUSQ_MODE": "garbage"}, "go live", "paper"),
    ],
)
def test_resolve_mode(
    ks: KillSwitch, tmp_path: Path, default: str, env: dict[str, str], go_live: str | None, expected: str
) -> None:
    if go_live is not None:
        arm(tmp_path, go_live)
    assert resolve_mode(default, env, ks) == expected
