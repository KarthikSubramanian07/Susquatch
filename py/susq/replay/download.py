"""Download public historical results and aggregate them to the county CSV the replay builder reads.

Source: MIT Election Data + Science Lab (MEDSL) official precinct returns on GitHub, which download
without a login (the county-level Dataverse files sit behind a guestbook form):

* 2022: `MEDSL/2022-elections-official/main/individual_states/2022-<st>-local-precinct-general.zip`
* 2024: `MEDSL/2024-elections-official/main/individual_states/<st>24.zip`

The Dataverse copies of the same data are CC0 1.0 (doi:10.7910/DVN/UYQIEP for 2022,
doi:10.7910/DVN/NYTPDU for 2024). Output goes to `data/replay/` (gitignored). Only statewide
contests are aggregated (Senate, Governor, President); House districts would need a district key.
"""

from __future__ import annotations

import csv
import io
import zipfile
from collections import defaultdict
from collections.abc import Iterable, Iterator
from pathlib import Path

import httpx

RAW = "https://raw.githubusercontent.com/MEDSL"
URLS = {
    2022: RAW + "/2022-elections-official/main/individual_states/2022-{st}-local-precinct-general.zip",
    2024: RAW + "/2024-elections-official/main/individual_states/{st}24.zip",
}
OFFICES = {"US SENATE", "GOVERNOR", "US PRESIDENT"}
NON_CANDIDATE = {"UNDERVOTES", "OVERVOTES", "TOTAL BALLOTS CAST", "BLANK", "OVER VOTES", "UNDER VOTES"}
HEADER = [
    "year",
    "state",
    "state_po",
    "county_name",
    "county_fips",
    "office",
    "district",
    "candidate",
    "party",
    "mode",
    "candidatevotes",
    "totalvotes",
]


def _party(p: str) -> str:
    p = (p or "").upper()
    return p if p in ("DEMOCRAT", "REPUBLICAN", "LIBERTARIAN") else "OTHER"


def aggregate_precincts(
    rows: Iterable[dict[str, str]], offices: set[str] = OFFICES
) -> list[dict[str, str | int]]:
    """Precinct rows (MEDSL schema) → county rows in `HEADER` order.

    Keeps general-election stage only, drops non-candidate rows and rows without a county FIPS
    ("STATE FLOATING" etc.), sums votes by county × candidate × mode, and sets `totalvotes` to the
    county's sum over kept rows (MEDSL's county-file definition).
    """
    votes: dict[tuple[str, ...], int] = defaultdict(int)
    for r in rows:
        office = r.get("office", "").upper()
        if office not in offices or r.get("stage", "GEN").upper() != "GEN":
            continue
        cand = r.get("candidate", "").strip().upper()
        fips = (r.get("county_fips") or "").strip()
        if cand in NON_CANDIDATE or not fips or not fips.replace(".0", "").isdigit():
            continue
        fips = fips.replace(".0", "").zfill(5)
        key = (
            r["year"],
            r["state"],
            r["state_po"],
            r["county_name"],
            fips,
            office,
            cand,
            _party(r.get("party_simplified", "")),
            r.get("mode", "TOTAL").upper() or "TOTAL",
        )
        votes[key] += int(float(r.get("votes") or 0))
    totals: dict[tuple[str, str], int] = defaultdict(int)
    for k, v in votes.items():
        totals[(k[4], k[5])] += v
    out: list[dict[str, str | int]] = []
    for k in sorted(votes):
        year, state, st, county, fips, office, cand, party, mode = k
        out.append(
            dict(
                year=year,
                state=state,
                state_po=st,
                county_name=county,
                county_fips=fips,
                office=office,
                district="STATEWIDE",
                candidate=cand,
                party=party,
                mode=mode,
                candidatevotes=votes[k],
                totalvotes=totals[(fips, office)],
            )
        )
    return out


def _csv_rows_from_zip(data: bytes) -> Iterator[dict[str, str]]:
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        for name in z.namelist():
            if name.lower().endswith(".csv") and not name.startswith("__MACOSX"):
                with z.open(name) as f:
                    yield from csv.DictReader(io.TextIOWrapper(f, encoding="utf-8", errors="replace"))


def download_state(year: int, state: str, client: httpx.Client) -> list[dict[str, str | int]]:
    """Fetch one state's precinct zip and aggregate it."""
    url = URLS[year].format(st=state.lower())
    resp = client.get(url, follow_redirects=True, timeout=120)
    resp.raise_for_status()
    return aggregate_precincts(_csv_rows_from_zip(resp.content))


def download(year: int, states: Iterable[str], out: Path) -> Path:
    """Download and aggregate `states` for `year` into one CSV at `out`."""
    out.parent.mkdir(parents=True, exist_ok=True)
    with (
        httpx.Client(headers={"User-Agent": "susquatch-replay/0.1"}) as client,
        out.open("w", newline="") as f,
    ):
        w = csv.DictWriter(f, fieldnames=HEADER)
        w.writeheader()
        for st in states:
            w.writerows(download_state(year, st, client))
    return out
