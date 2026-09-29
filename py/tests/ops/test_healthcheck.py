from __future__ import annotations

import os
from datetime import UTC, datetime, timedelta

from susq.ops.healthcheck import MAX_SILENCE_S, bridge, main

OPEN = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)
AFTER = OPEN + timedelta(hours=3)


def touch(path, when: datetime) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("x")
    os.utime(path, (when.timestamp(), when.timestamp()))


def test_waiting_for_the_key_is_healthy(home) -> None:
    (home / ".env").write_text("SUSQ_API_KEY=\n")
    assert bridge(home, {}, AFTER) == (True, "waiting for SUSQ_API_KEY")


def test_key_in_dotenv_counts_and_env_wins(home) -> None:
    (home / ".env").write_text("SUSQ_API_KEY=abc\n")
    assert bridge(home, {}, OPEN - timedelta(hours=1)) == (True, "waiting for the open")
    assert bridge(home, {"SUSQ_API_KEY": ""}, AFTER)[1] == "waiting for SUSQ_API_KEY"


def test_after_open_needs_recent_status_or_journal(home) -> None:
    env = {"SUSQ_API_KEY": "abc"}
    ok, why = bridge(home, env, AFTER)
    assert not ok
    assert "since the open" in why

    touch(home / "journal" / "2026-10-01" / "bridge.journal.jsonl", AFTER - timedelta(seconds=30))
    assert bridge(home, env, AFTER)[0] is True  # measure_day1 is journaling, no status yet

    later = AFTER + timedelta(seconds=MAX_SILENCE_S + 60)
    ok, why = bridge(home, env, later)
    assert not ok
    assert "no status or journal activity" in why

    touch(home / "state" / "status.json", later - timedelta(seconds=5))
    assert bridge(home, env, later)[0] is True


def test_yesterdays_journal_covers_midnight(home) -> None:
    env = {"SUSQ_API_KEY": "abc"}
    just_after_midnight = datetime(2026, 10, 3, 0, 1, tzinfo=UTC)
    touch(
        home / "journal" / "2026-10-02" / "bridge.journal.jsonl", just_after_midnight - timedelta(minutes=2)
    )
    assert bridge(home, env, just_after_midnight)[0] is True


def test_cli_usage(capsys) -> None:
    assert main(["nope"]) == 2
    assert "usage" in capsys.readouterr().err
