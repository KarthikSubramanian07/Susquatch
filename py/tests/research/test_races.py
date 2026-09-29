"""The shared race/party parser: every venue's phrasing lands on the same key; non-winners rejected."""

from __future__ import annotations

import pytest
from susq.mapping import geo
from susq.mapping.races import DEM, IND, REP, parse_party, parse_race, race_from_id


@pytest.mark.parametrize(
    ("text", "race_id"),
    [
        # Cup
        ("Will the Democratic Party win the New Hampshire Senate?", "2026-SEN-NH"),
        ("Will the Republican Party win the Ohio Senate?", "2026-SEN-OH-S"),
        ("Will the Democratic Party win the House?", "2026-CONTROL-HOUSE"),
        ("Will the Republican Party win the Senate?", "2026-CONTROL-SENATE"),
        ("Will the Democratic Party win the Georgia Governor race?", "2026-GOV-GA"),
        ("Will the Democratic Party win NY-17?", "2026-HOUSE-NY-17"),
        ("Will the Democratic Party win Pennsylvania's 7th Congressional District?", "2026-HOUSE-PA-07"),
        ("Will Dan Osborn (Independent) win the Nebraska Senate?", "2026-SEN-NE"),
        # Kalshi
        ("Will Democratics win the Senate race in New Hampshire?", "2026-SEN-NH"),
        ("Will Democrats win the U.S. Senate in 2026?", "2026-CONTROL-SENATE"),
        ("Will Democrats win the House in 2026?", "2026-CONTROL-HOUSE"),
        ("Will Republican win the House race for AK-AL?", "2026-HOUSE-AK-AL"),
        ("Will the Democratic party win the governorship in Maine", "2026-GOV-ME"),
        ("Will Republicans win the Senate race in West Virginia?", "2026-SEN-WV"),
        ("Will Republicans win the Senate race in Kansas?", "2026-SEN-KS"),
        ("Alaska House winner?", "2026-HOUSE-AK-AL"),
        # Polymarket
        ("Will the Democrats win the Nebraska Senate race in 2026?", "2026-SEN-NE"),
        (
            "Will the Democratic Party control the Senate after the 2026 Midterm elections?",
            "2026-CONTROL-SENATE",
        ),
        ("Will the Republican Party win the CA-22 House seat?", "2026-HOUSE-CA-22"),
        ("Will the Democrats win the Florida Senate race in 2026?", "2026-SEN-FL-S"),
    ],
)
def test_parse_race(text: str, race_id: str) -> None:
    race = parse_race(text)
    assert race is not None
    assert race.race_id == race_id


@pytest.mark.parametrize(
    "text",
    [
        "Will the Republican Party candidate win the 2026 New Hampshire Senate election by 6% or more?",
        "Will total votes cast in the 2026 midterm election for Senator from Nebraska be less than 650,000?",
        "Will ACA premium tax credits not be extended and will the Democratic Party win the House in 2026?",
        "Will the Republican Party hold 47 or fewer Senate seats after the 2026 midterm elections?",
        "Will Republicans win any Senate or Governor's Election in a Biden-Trump state?",
        "Will Alaska have the closest Senate race in 2026?",
        "Will a candidate not listed above win the New Hampshire Senate race in 2026?",
        "Republican margin: Nebraska vs. Kansas Senate",
        "New Hampshire Senate winner? (2028)",
        "Will Hakeem Jeffries be the first Speaker of the House after the 2026 midterms?",
        "Will total 2026 midterm turnout exceed 2018?",
        "Will the Democrats win the Georgia Senate race in Texas?",  # two states: ambiguous
    ],
)
def test_non_winner_markets_are_rejected(text: str) -> None:
    assert parse_race(text) is None


@pytest.mark.parametrize(
    ("text", "party"),
    [
        ("Will the Democratic Party win the House?", DEM),
        ("Will Democratics win the Senate race in Ohio?", DEM),
        ("Will Republicans win the Senate race in Iowa?", REP),
        ("Will an independent win the Nebraska Senate race in 2026?", IND),
        ("Will Dan Osborn (as an independent)s win the Senate race in Nebraska?", IND),
        ("Will Todd Achilles win the Idaho Senate race in 2026?", None),
        ("Democrats vs Republicans?", None),
    ],
)
def test_parse_party(text: str, party: str | None) -> None:
    assert parse_party(text) == party


@pytest.mark.parametrize(
    "race_id",
    [
        "2026-SEN-NH",
        "2026-SEN-OH-S",
        "2026-GOV-GA",
        "2026-HOUSE-NY-17",
        "2026-HOUSE-AK-AL",
        "2026-CONTROL-HOUSE",
    ],
)
def test_race_id_roundtrip(race_id: str) -> None:
    assert race_from_id(race_id).race_id == race_id


@pytest.mark.parametrize("bad", ["2028-SEN-NH", "2026-SEN-NH-S", "2026-SEN-OH", "2026-XYZ-NH", "garbage"])
def test_race_from_id_rejects_malformed(bad: str) -> None:
    with pytest.raises(ValueError, match="not a race id"):
        race_from_id(bad)


def test_senate_map_matches_the_2026_chamber() -> None:
    assert len(geo.SENATE_SEATS_UP) == 35
    assert (geo.SENATE_D_NOT_UP, geo.SENATE_R_NOT_UP) == (34, 31)
    assert geo.SENATE_D_NOT_UP + geo.SENATE_R_NOT_UP + 35 == 100
    # Democrats need a net +4 (RESEARCH §5): 13 held + 4 = 17 wins => 34 + 17 = 51.
    held = sum(p == "DEM" for p in geo.SENATE_SEATS_UP.values())
    assert geo.SENATE_D_NOT_UP + held + 4 == geo.SENATE_D_CONTROL_SEATS


def test_regions_partition_the_states_and_clusters_tag_correctly() -> None:
    all_states = [s for states in geo.REGIONS.values() for s in states]
    assert sorted(all_states) == sorted(geo.STATE_NAMES)
    assert geo.clusters_for("OH", house=False) == [
        "national", "region:midwest", "state:OH", "red-state-reach", "midwest-industrial",
    ]  # fmt: skip
    assert geo.clusters_for("NY", house=True)[-1] == "house"
    assert geo.is_early_reporting("GA")
    assert not geo.is_early_reporting("AK")
