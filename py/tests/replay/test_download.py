"""Precinct → county aggregation (network download itself is not exercised offline)."""

from __future__ import annotations

from susq.replay.download import aggregate_precincts


def _row(**kw: str) -> dict[str, str]:
    base = {
        "year": "2022",
        "state": "GEORGIA",
        "state_po": "GA",
        "county_name": "APPLING",
        "county_fips": "13001",
        "office": "US SENATE",
        "candidate": "RAPHAEL WARNOCK",
        "party_simplified": "DEMOCRAT",
        "mode": "ABSENTEE",
        "votes": "10",
        "stage": "GEN",
    }
    base.update(kw)
    return base


def test_aggregates_filters_and_totals() -> None:
    rows = [
        _row(),
        _row(votes="5"),  # second precinct, same county/mode
        _row(mode="ELECTION DAY", votes="7"),
        _row(candidate="HERSCHEL WALKER", party_simplified="REPUBLICAN", votes="20"),
        _row(candidate="UNDERVOTES", votes="99"),
        _row(stage="GEN RUNOFF", votes="1000"),
        _row(county_fips="", county_name="STATE FLOATING", votes="50"),
        _row(office="US HOUSE", votes="3"),
        _row(county_fips="13001.0", candidate="CHASE OLIVER", party_simplified="LIBERTARIAN", votes="2.0"),
    ]
    out = aggregate_precincts(rows)
    by = {(r["candidate"], r["mode"]): r for r in out}
    assert by[("RAPHAEL WARNOCK", "ABSENTEE")]["candidatevotes"] == 15
    assert by[("RAPHAEL WARNOCK", "ELECTION DAY")]["candidatevotes"] == 7
    assert by[("CHASE OLIVER", "ABSENTEE")]["party"] == "LIBERTARIAN"
    assert all(r["totalvotes"] == 15 + 7 + 20 + 2 for r in out)
    assert {r["county_fips"] for r in out} == {"13001"}
    assert len(out) == 4
