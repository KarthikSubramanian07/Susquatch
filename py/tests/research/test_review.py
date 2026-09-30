"""Rules review: what counts as clean, whole-race approval, and the checklist report."""

from __future__ import annotations

import pytest
from susq.contracts import ops_pb2
from susq.mapping.review import clean_races, render, risks, verify

EXACT = "confidence=1.00; kalshi=exact(x); polymarket=exact(x)"


def _row(mid: str, race: str, party: str, **kw) -> ops_pb2.MarketRule:
    base = {"market_id": mid, "exchange_id": f"x{mid}", "title": f"{party} {race}", "race_id": race}
    base |= {"kind": "race_party", "party": party, "notes": EXACT}
    return ops_pb2.MarketRule(**(base | kw))


def _map() -> ops_pb2.MarketMap:
    return ops_pb2.MarketMap(
        markets=[
            _row("1", "2026-SEN-RI", "DEM"),
            _row("2", "2026-SEN-RI", "REP"),
            _row("3", "2026-SEN-NE", "DEM", independent_can_win=True),
            _row("4", "2026-SEN-NE", "REP", independent_can_win=True),
            _row("5", "2026-GOV-GA", "DEM", runoff_possible=True),
            _row("6", "2026-GOV-GA", "REP", runoff_possible=True),
            _row("7", "2026-SEN-OH", "DEM"),
            _row("8", "2026-SEN-OH", "REP", notes="confidence=1.00; kalshi=exact(x); polymarket=none"),
            _row("9", "2026-CONTROL-HOUSE", "DEM", kind="chamber", manual_settlement=True),
        ]
    )


def test_risks_name_each_reason() -> None:
    mm = _map()
    by = {r.market_id: risks(r) for r in mm.markets}
    assert by["1"] == []
    assert by["3"] == ["independent can win"]
    assert by["5"] == ["runoff"]
    assert by["8"] == ["one venue only"]
    assert by["9"] == ["chamber", "manual settlement"]


def test_only_whole_clean_two_party_races_are_clean() -> None:
    assert clean_races(_map()) == ["2026-SEN-RI"]


def test_verify_by_race_and_market_and_rejects_unknown_ids() -> None:
    mm = _map()
    assert verify(mm, race_ids=["2026-SEN-NE"], market_ids=["9"]) == ["3", "4", "9"]
    assert verify(mm, race_ids=["2026-SEN-NE"]) == []  # already verified: nothing changes
    with pytest.raises(KeyError, match="2026-SEN-XX"):
        verify(mm, race_ids=["2026-SEN-XX"])
    assert {r.market_id for r in mm.markets if r.verified} == {"3", "4", "9"}


def test_checklist_puts_risky_races_first_and_ticks_verified_rows() -> None:
    mm = _map()
    verify(mm, race_ids=["2026-SEN-RI"])
    text = render(mm)
    assert text.index("2026-GOV-GA") < text.index("2026-SEN-RI")
    assert "| x | 2026-SEN-RI | 1 | DEM |" in text
    assert "Verified rows: 2. Clean races: 1." in text
