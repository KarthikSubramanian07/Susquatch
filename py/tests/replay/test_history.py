from __future__ import annotations

import pytest
from susq.replay.history import (
    EARLY,
    EDAY,
    MAIL,
    MIXED,
    PROVISIONAL,
    ContestHistory,
    classify_mode,
    load_contest,
)

from .conftest import SAMPLE_CSV


def test_ga_2022_senate_matches_certified_totals(ga_senate: ContestHistory) -> None:
    # Georgia SoS certified general (first round): Warnock 1,946,117; Walker 1,908,442; Oliver 81,365.
    t = ga_senate.tally
    assert (t.dem, t.rep, t.other) == (1_946_117, 1_908_442, 81_365)
    assert len(ga_senate.counties) == 159
    assert t.margin_pts() == pytest.approx(0.977, abs=1e-3)


def test_ga_counties_are_split_by_vote_type(ga_senate: ContestHistory) -> None:
    fulton = ga_senate.counties["13121"]
    assert fulton.is_split
    assert set(fulton.by_type) == {MAIL, EARLY, EDAY, PROVISIONAL}
    # Mail skews D relative to E-day in Fulton.
    assert fulton.by_type[MAIL].margin_pts() > fulton.by_type[EDAY].margin_pts()


def test_unsplit_states_are_mixed(nh_senate: ContestHistory) -> None:
    assert len(nh_senate.counties) == 10
    assert all(set(c.by_type) == {MIXED} for c in nh_senate.counties.values())


def test_total_rows_are_dropped_when_split_rows_exist(tmp_path) -> None:
    csv = tmp_path / "x.csv"
    header = (
        "year,state,state_po,county_name,county_fips,office,district,candidate,party,mode,"
        "candidatevotes,totalvotes\n"
    )
    rows = [
        "2022,GEORGIA,GA,A,13001,US SENATE,STATEWIDE,D,DEMOCRAT,ABSENTEE,10,30",
        "2022,GEORGIA,GA,A,13001,US SENATE,STATEWIDE,D,DEMOCRAT,ELECTION DAY,5,30",
        "2022,GEORGIA,GA,A,13001,US SENATE,STATEWIDE,D,DEMOCRAT,TOTAL,15,30",
        "2022,GEORGIA,GA,A,13001,US SENATE,STATEWIDE,R,REPUBLICAN,TOTAL,15,30",
    ]
    csv.write_text(header + "\n".join(rows) + "\n")
    c = load_contest(csv, year=2022, state="GA", office="US SENATE").counties["13001"]
    assert c.tally.dem == 15, "TOTAL must not double-count the split rows"


def test_missing_contest_raises() -> None:
    with pytest.raises(ValueError, match="no rows"):
        load_contest(SAMPLE_CSV, year=2022, state="GA", office="GOVERNOR")


@pytest.mark.parametrize(
    ("label", "expected"),
    [
        ("ABSENTEE", MAIL),
        ("ABSENTEE BY MAIL", MAIL),
        ("ADVANCED", EARLY),
        ("ONE STOP", EARLY),
        ("ELECTION DAY", EDAY),
        ("FAILSAFE", EDAY),
        ("PROVISIONAL", PROVISIONAL),
        ("PROVIONSAL", PROVISIONAL),
        ("UNSPECIFIED", MIXED),
    ],
)
def test_mode_labels(label: str, expected: int) -> None:
    assert classify_mode(label) == expected
