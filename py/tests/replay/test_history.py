from __future__ import annotations

from pathlib import Path

import pytest
from susq.replay.history import (
    EARLY,
    EDAY,
    MAIL,
    MIXED,
    PROVISIONAL,
    ContestHistory,
    ContestKey,
    classify_mode,
    collapse_to_unit,
    district_unit_id,
    is_unit_key,
    load_all,
    load_contest,
    party_code,
    pool_independent,
    top_independent,
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


HEADER = (
    "year,state,state_po,county_name,county_fips,office,district,candidate,party,mode,"
    "candidatevotes,totalvotes\n"
)


def _csv(path: Path, rows: list[str]) -> Path:
    path.write_text(HEADER + "\n".join(rows) + "\n")
    return path


def test_multiple_files_and_district_selection(tmp_path: Path) -> None:
    a = _csv(
        tmp_path / "a.csv",
        [
            "2022,GEORGIA,GA,A,13001,US SENATE,STATEWIDE,D,DEMOCRAT,TOTAL,60,100",
            "2022,GEORGIA,GA,A,13001,US SENATE,STATEWIDE,R,REPUBLICAN,TOTAL,40,100",
            "2022,GEORGIA,GA,A,13001,US HOUSE,01,HD,DEMOCRAT,TOTAL,10,30",
            "2022,GEORGIA,GA,A,13001,US HOUSE,01,HR,REPUBLICAN,TOTAL,20,30",
            "2022,GEORGIA,GA,A,13001,US HOUSE,02,HD2,DEMOCRAT,TOTAL,7,7",
        ],
    )
    b = _csv(
        tmp_path / "b.csv",
        [
            "2024,GEORGIA,GA,A,13001,US PRESIDENT,,D,DEMOCRAT,TOTAL,70,120",  # blank district = statewide
            "2024,GEORGIA,GA,A,13001,US PRESIDENT,,R,REPUBLICAN,TOTAL,50,120",
        ],
    )
    sen = load_contest([a, b], year=2022, state="ga", office="us senate")
    assert sen.tally.total == 100
    pres = load_contest([a, b], year=2024, state="GA", office="US PRESIDENT")
    assert (pres.tally.dem, pres.tally.rep) == (70, 50)
    h1 = load_contest(a, year=2022, state="GA", office="US HOUSE", district="01")
    assert (h1.tally.dem, h1.tally.rep) == (10, 20)
    with pytest.raises(ValueError, match="no rows"):
        load_contest(a, year=2022, state="GA", office="US HOUSE")  # House needs a district

    every = load_all([a, b])
    assert set(every) == {
        ContestKey(2022, "GA", "US SENATE"),
        ContestKey(2022, "GA", "US HOUSE", "01"),
        ContestKey(2022, "GA", "US HOUSE", "02"),
        ContestKey(2024, "GA", "US PRESIDENT"),
    }
    assert every[ContestKey(2022, "GA", "US HOUSE", "02")].tally.dem == 7


def test_pool_independent_joins_the_democrat(tmp_path: Path) -> None:
    p = _csv(
        tmp_path / "ne.csv",
        [
            "2024,NEBRASKA,NE,A,31001,US SENATE,STATEWIDE,FISCHER,REPUBLICAN,TOTAL,55,100",
            "2024,NEBRASKA,NE,A,31001,US SENATE,STATEWIDE,OSBORN,OTHER,TOTAL,40,100",
            "2024,NEBRASKA,NE,A,31001,US SENATE,STATEWIDE,MINOR,OTHER,TOTAL,5,100",
        ],
    )
    c = load_contest(p, year=2024, state="NE", office="US SENATE")
    assert c.tally.dem == 0
    assert top_independent(c) == ("OSBORN", pytest.approx(0.40))
    pooled = pool_independent(c)
    t = pooled.tally
    assert (t.dem, t.rep, t.other) == (40, 55, 5)
    assert pooled.counties["31001"].candidates["OSBORN"][0] == "INDEPENDENT"
    assert party_code("INDEPENDENT") == "IND"
    assert c.tally.dem == 0, "the input contest is not mutated"
    plain = load_contest(SAMPLE_CSV, year=2022, state="NH", office="US SENATE")
    assert pool_independent(plain).tally.dem >= plain.tally.dem


def test_fusion_lines_count_for_the_major_party(tmp_path: Path) -> None:
    p = _csv(
        tmp_path / "ny.csv",
        [
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,HOCHUL,DEMOCRAT,TOTAL,50,100",
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,HOCHUL,OTHER,TOTAL,10,100",  # Working Families line
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,ZELDIN,REPUBLICAN,TOTAL,30,100",
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,ZELDIN,OTHER,TOTAL,8,100",  # Conservative line
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,MINOR,OTHER,TOTAL,2,100",
        ],
    )
    t = load_contest(p, year=2022, state="NY", office="GOVERNOR").tally
    assert (t.dem, t.rep, t.other) == (60, 38, 2)


def test_ballot_count_rows_are_not_candidates(tmp_path: Path) -> None:
    p = _csv(
        tmp_path / "ny.csv",
        [
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,HOCHUL,DEMOCRAT,TOTAL,50,100",
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,ZELDIN,REPUBLICAN,TOTAL,30,100",
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,PUBLIC COUNTER,OTHER,TOTAL,900,100",
            "2022,NEW YORK,NY,A,36001,GOVERNOR,STATEWIDE,SCATTERING,OTHER,TOTAL,3,100",  # real write-ins
        ],
    )
    c = load_contest(p, year=2022, state="NY", office="GOVERNOR")
    assert c.tally.other == 3
    assert "PUBLIC COUNTER" not in c.counties["36001"].candidates


def test_district_units_collapse_a_district_split_across_counties(tmp_path: Path) -> None:
    a = _csv(
        tmp_path / "ca.csv",
        [
            "2022,CALIFORNIA,CA,KERN,06029,US HOUSE,22,VALADAO,REPUBLICAN,TOTAL,35000,65000",
            "2022,CALIFORNIA,CA,KERN,06029,US HOUSE,22,SALAS,DEMOCRAT,TOTAL,30000,65000",
            "2022,CALIFORNIA,CA,KINGS,06031,US HOUSE,22,VALADAO,REPUBLICAN,TOTAL,12000,22000",
            "2022,CALIFORNIA,CA,KINGS,06031,US HOUSE,22,SALAS,DEMOCRAT,TOTAL,10000,22000",
        ],
    )
    pieces = load_contest(a, year=2022, state="CA", office="US HOUSE", district="22")
    assert len(pieces.counties) == 2
    unit = collapse_to_unit(pieces, district_unit_id("CA", "22"), "CA district 22")
    assert list(unit.counties) == ["CA-HD22"]
    (u,) = unit.counties.values()
    assert (u.county_fips, u.state, u.county_name) == ("CA-HD22", "CA", "CA district 22")
    assert (u.tally.dem, u.tally.rep, u.tally.total) == (40_000, 47_000, 87_000)
    assert u.candidates["VALADAO"] == ("REPUBLICAN", {MIXED: 47_000})
    assert not u.is_split
    assert (unit.year, unit.state, unit.office) == (2022, "CA", "US HOUSE")
    assert pieces.counties["06029"].tally.total == 65_000, "the input is not modified"

    # California's ABSENTEE / NOT ABSENTEE modes (both read as mail) and a TOTAL-only county
    # fold into one MIXED tally: the feed reports the district untyped.
    b = _csv(
        tmp_path / "ca_split.csv",
        [
            "2022,CALIFORNIA,CA,KERN,06029,US HOUSE,22,V,REPUBLICAN,ABSENTEE,20000,65000",
            "2022,CALIFORNIA,CA,KERN,06029,US HOUSE,22,V,REPUBLICAN,NOT ABSENTEE,15000,65000",
            "2022,CALIFORNIA,CA,KERN,06029,US HOUSE,22,S,DEMOCRAT,ABSENTEE,25000,65000",
            "2022,CALIFORNIA,CA,KERN,06029,US HOUSE,22,S,DEMOCRAT,NOT ABSENTEE,5000,65000",
            "2022,CALIFORNIA,CA,KINGS,06031,US HOUSE,22,V,REPUBLICAN,TOTAL,12000,22000",
            "2022,CALIFORNIA,CA,KINGS,06031,US HOUSE,22,S,DEMOCRAT,TOTAL,10000,22000",
        ],
    )
    split = load_contest(b, year=2022, state="CA", office="US HOUSE", district="22")
    assert {vt for c in split.counties.values() for vt in c.by_type} == {MAIL, MIXED}
    (folded,) = collapse_to_unit(split, "CA-HD22").counties.values()
    assert list(folded.by_type) == [MIXED]
    assert (folded.tally.dem, folded.tally.rep) == (40_000, 47_000)
    assert folded.candidates["S"] == ("DEMOCRAT", {MIXED: 40_000})


def test_unit_ids_match_the_rust_pipeline() -> None:
    assert district_unit_id("CA", "22") == "CA-HD22"
    assert district_unit_id("ca", "1") == "CA-HD01"
    assert district_unit_id("WY", "al") == "WY-HDAL"
    for bad in ("0", "100", "X", ""):
        with pytest.raises(ValueError, match="not a House district"):
            district_unit_id("CA", bad)
    assert is_unit_key("06037", "CA")
    assert is_unit_key("CA-HD22", "CA")
    assert is_unit_key("CA-HDAL", "CA")
    for bad in ("CA-HD22@NV", "CA-HD2", "CA-HD00", "CA-22", "0603", ""):
        state = "NV" if bad.endswith("@NV") else "CA"
        assert not is_unit_key(bad.removesuffix("@NV"), state), bad
