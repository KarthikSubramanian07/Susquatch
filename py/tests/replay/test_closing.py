"""config/poll_closing.toml: complete, internally consistent, and UTC math right."""

from __future__ import annotations

from datetime import UTC, datetime
from zoneinfo import ZoneInfo

from susq.replay.closing import StateClosing

BATTLEGROUND = ("CA", "OH", "MI", "TX", "IA", "ME", "NH", "NC", "GA", "AK", "AZ", "NV", "NE", "KS", "SC")


def test_every_state_present(closings: dict[str, StateClosing]) -> None:
    assert len(closings) == 50
    assert all(len(st) == 2 and st.isupper() for st in closings)
    assert "DC" not in closings
    assert set(BATTLEGROUND) <= set(closings)


def test_utc_times_match_tzdata_and_are_ordered(closings: dict[str, StateClosing]) -> None:
    for st, sc in closings.items():
        prev = None
        for c in sc.closings:
            h, m = map(int, c.closes_local.split(":"))
            local = datetime(2026, 11, 3, h, m, tzinfo=ZoneInfo(c.tz))
            assert local.astimezone(UTC) == c.closes_utc, (st, c)
            assert prev is None or c.closes_utc >= prev, st
            prev = c.closes_utc


def test_known_battleground_closes(closings: dict[str, StateClosing]) -> None:
    # 7pm ET = 00:00Z (EST, after DST ends Nov 1); split-zone states close last in the later zone.
    assert closings["GA"].last_close_utc == datetime(2026, 11, 4, 0, 0, tzinfo=UTC)
    assert closings["OH"].last_close_utc == datetime(2026, 11, 4, 0, 30, tzinfo=UTC)
    assert closings["TX"].last_close_utc == datetime(2026, 11, 4, 2, 0, tzinfo=UTC)
    assert closings["MI"].last_close_utc == datetime(2026, 11, 4, 2, 0, tzinfo=UTC)
    assert closings["CA"].last_close_utc == datetime(2026, 11, 4, 4, 0, tzinfo=UTC)
    assert closings["AK"].last_close_utc == datetime(2026, 11, 4, 6, 0, tzinfo=UTC)
    assert all(sc.source.startswith("http") for sc in closings.values())
