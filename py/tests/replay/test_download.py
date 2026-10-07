"""Precinct → county aggregation and the per-state cache (no network: the fetcher is faked)."""

from __future__ import annotations

import csv
from pathlib import Path

import httpx
import pytest
from susq.replay.download import aggregate_precincts, cache_path, download, normalize_district


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


def test_suppressed_votes_and_alaska_districts() -> None:
    rows = [
        _row(votes="*"),
        _row(votes="4"),
        _row(
            state_po="AK",
            county_fips="",
            county_name="",
            jurisdiction_fips="02001",
            jurisdiction_name="DISTRICT 01",
        ),
    ]
    out = aggregate_precincts(rows)
    by = {r["county_fips"]: r for r in out}
    assert by["13001"]["candidatevotes"] == 4
    assert by["02001"]["county_name"] == "DISTRICT 01"


def test_house_rows_are_kept_by_district() -> None:
    rows = [
        _row(),
        _row(office="US HOUSE", district="7", candidate="A", votes="30"),
        _row(office="US HOUSE", district="007", candidate="B", party_simplified="REPUBLICAN", votes="20"),
        _row(office="US HOUSE", district="8", candidate="C", votes="5"),
    ]
    assert {r["office"] for r in aggregate_precincts(rows)} == {"US SENATE"}
    out = aggregate_precincts(rows, house=True)
    house = {(r["district"], r["candidate"]): r for r in out if r["office"] == "US HOUSE"}
    assert set(house) == {("07", "A"), ("07", "B"), ("08", "C")}
    assert house[("07", "A")]["totalvotes"] == 50, "totals are per county x district"
    assert house[("08", "C")]["totalvotes"] == 5
    assert next(r for r in out if r["office"] == "US SENATE")["district"] == "STATEWIDE"


@pytest.mark.parametrize(
    ("raw", "expected"),
    [("7", "07"), ("007", "07"), ("12.0", "12"), ("0", "AL"), ("AT-LARGE", "AL"), ("", "STATEWIDE")],
)
def test_normalize_district(raw: str, expected: str) -> None:
    assert normalize_district(raw) == expected


class _FakeFetcher:
    def __init__(self, fail: set[str], empty: set[str] = frozenset()) -> None:  # type: ignore[assignment]
        self.fail, self.empty, self.calls = fail, empty, []

    def __call__(self, year: int, state: str, client: object) -> list[dict[str, str | int]]:
        self.calls.append(state)
        if state in self.fail:
            raise httpx.HTTPStatusError("404 Not Found", request=httpx.Request("GET", "x"), response=None)  # type: ignore[arg-type]
        if state in self.empty:
            return []
        return aggregate_precincts([_row(state_po=state, county_fips="13001" if state == "GA" else "33001")])


def test_download_caches_per_state_and_reports_failures(tmp_path: Path) -> None:
    out = tmp_path / "replay" / "county_results_2022.csv"
    fake = _FakeFetcher(fail={"XX"}, empty={"YY"})
    res = download(2022, ["ga", "NH", "XX", "YY"], out, fetch=fake)
    assert res.done == ["GA", "NH"]
    assert res.cached == []
    assert set(res.failed) == {"XX"}
    assert "HTTPStatusError" in res.failed["XX"]
    assert res.empty == ["YY"], "a state without the offices that year is not a failure"
    assert cache_path(out, 2022, "GA").exists()
    assert not cache_path(out, 2022, "XX").exists()
    with out.open() as f:
        merged = list(csv.DictReader(f))
    assert {r["state_po"] for r in merged} == {"GA", "NH"}

    # Second run: cached states are not fetched again; the failed one is retried.
    again = _FakeFetcher(fail=set())
    res2 = download(2022, ["GA", "NH", "XX"], out, fetch=again)
    assert again.calls == ["XX"]
    assert res2.cached == ["GA", "NH"]
    assert res2.done == ["GA", "NH", "XX"]
    assert not res2.failed
    with out.open() as f:
        assert {r["state_po"] for r in csv.DictReader(f)} == {"GA", "NH", "XX"}

    fresh = _FakeFetcher(fail=set())
    download(2022, ["GA"], out, fetch=fresh, refresh=True)
    assert fresh.calls == ["GA"]
