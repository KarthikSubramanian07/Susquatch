"""Download public historical results and aggregate them to the county CSV the replay builder reads.

Source: MIT Election Data + Science Lab (MEDSL) official precinct returns on GitHub, which download
without a login (the county-level Dataverse files sit behind a guestbook form):

* 2022: `MEDSL/2022-elections-official/main/individual_states/2022-<st>-local-precinct-general.zip`
* 2024: `MEDSL/2024-elections-official/main/individual_states/<st>24.zip`

The Dataverse copies of the same data are CC0 1.0 (doi:10.7910/DVN/UYQIEP for 2022,
doi:10.7910/DVN/NYTPDU for 2024). Output goes to `data/replay/` (gitignored).

Statewide contests (Senate, Governor, President) are aggregated by county with `district =
"STATEWIDE"`. With `house=True`, US House rows are kept too and aggregated by county x district
(`district = "07"`, ...); `house_only=True` keeps *only* those rows, for a House history file that
sits next to the statewide one without duplicating its contests. The 2026 spec builder uses a
district's total (summed over its counties) for California House seats, which the SoS feed reports
as one unit per district; elsewhere a county split across districts cannot be modelled. Each state
is cached as its own CSV under `<out dir>/medsl/<year>/` (`<ST>.csv`, `<ST>.house.csv` for
house-only, `<ST>.with-house.csv` for both) so a failed state never loses the others and a cached
file is never reused for another selection; `download()` reports failures instead of raising.
"""

from __future__ import annotations

import csv
import io
import logging
import zipfile
from collections import defaultdict
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass, field
from functools import partial
from pathlib import Path

import httpx

from susq.replay.history import NON_CANDIDATE

log = logging.getLogger(__name__)

RAW = "https://raw.githubusercontent.com/MEDSL"
URLS = {
    2022: RAW + "/2022-elections-official/main/individual_states/2022-{st}-local-precinct-general.zip",
    2024: RAW + "/2024-elections-official/main/individual_states/{st}24.zip",
}
OFFICES = {"US SENATE", "GOVERNOR", "US PRESIDENT"}
HOUSE_OFFICE = "US HOUSE"
STATEWIDE = "STATEWIDE"
SPECIAL_SUFFIX = " SPECIAL"  # e.g. "US SENATE SPECIAL" (OK 2022 ran a regular and a special Senate race)
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


def _party(simplified: str, detailed: str = "") -> str:
    """MEDSL `party_simplified`, falling back to `party_detailed` where the simplified label is
    missing or wrong (MN 2022 DFL and VT "DEMOCRAT/PROGRESSIVE" are simplified to OTHER, NE 2024
    President to NONPARTISAN)."""
    p = (simplified or "").strip().upper()
    if p in ("DEMOCRAT", "REPUBLICAN", "LIBERTARIAN"):
        return p
    d = (detailed or "").strip().upper()
    if "DEMOCRAT" in d and "REPUBLICAN" not in d:
        return "DEMOCRAT"
    if "REPUBLICAN" in d and "DEMOCRAT" not in d:
        return "REPUBLICAN"
    return "LIBERTARIAN" if d == "LIBERTARIAN" else "OTHER"


def _votes(v: str | None) -> int:
    """MEDSL vote counts; suppressed cells ("*", "") count as 0."""
    try:
        return int(float(v or 0))
    except ValueError:
        return 0


def _county(r: dict[str, str]) -> tuple[str, str]:
    """(county FIPS, county name). Alaska has no counties: MEDSL leaves `county_fips` blank and
    keys precincts by state house district in `jurisdiction_fips` (02001-02040), which is used
    instead (a needle prior for AK only matches a feed that reports by the same district code)."""
    fips = (r.get("county_fips") or "").strip()
    if not fips and r.get("state_po", "").upper() == "AK":
        return (r.get("jurisdiction_fips") or "").strip(), (r.get("jurisdiction_name") or "").strip()
    return fips, r.get("county_name", "")


def normalize_district(d: str) -> str:
    """MEDSL district labels ("7", "007", "AL", "At-Large") → "07" / "AL"."""
    s = (d or "").strip().upper().replace(".0", "")
    if s in {"", "STATEWIDE"}:
        return STATEWIDE
    if s.isdigit():
        return f"{int(s):02d}" if int(s) else "AL"
    return "AL" if "LARGE" in s or s == "AL" else s


def aggregate_precincts(
    rows: Iterable[dict[str, str]],
    offices: set[str] = OFFICES,
    *,
    house: bool = False,
    house_only: bool = False,
) -> list[dict[str, str | int]]:
    """Precinct rows (MEDSL schema) → county rows in `HEADER` order.

    Keeps general-election stage only, drops non-candidate rows and rows without a county FIPS
    ("STATE FLOATING" etc.), sums votes by county × candidate × mode, and sets `totalvotes` to the
    county's sum over kept rows (MEDSL's county-file definition). Statewide offices carry
    `district = "STATEWIDE"`; with `house=True`, `US HOUSE` rows are kept and keyed by district
    as well, with `totalvotes` per county × district; `house_only=True` keeps only those.
    """
    keep = {HOUSE_OFFICE} if house_only else set(offices) | ({HOUSE_OFFICE} if house else set())
    votes: dict[tuple[str, ...], int] = defaultdict(int)
    for r in rows:
        office = r.get("office", "").upper()
        if office not in keep or r.get("stage", "GEN").upper() != "GEN":
            continue
        if office != HOUSE_OFFICE and r.get("special", "").strip().upper() == "TRUE":
            office += SPECIAL_SUFFIX  # a second race for the same office: keep it apart
        cand = r.get("candidate", "").strip().upper()
        fips, county = _county(r)
        if cand in NON_CANDIDATE or not fips or not fips.replace(".0", "").isdigit():
            continue
        fips = fips.replace(".0", "").zfill(5)
        district = normalize_district(r.get("district", "")) if office == HOUSE_OFFICE else STATEWIDE
        key = (
            r["year"],
            r["state"],
            r["state_po"],
            county,
            fips,
            office,
            district,
            cand,
            _party(r.get("party_simplified", ""), r.get("party_detailed", "")),
            r.get("mode", "TOTAL").upper() or "TOTAL",
        )
        votes[key] += _votes(r.get("votes"))
    totals: dict[tuple[str, str, str], int] = defaultdict(int)
    for k, v in votes.items():
        totals[(k[4], k[5], k[6])] += v
    out: list[dict[str, str | int]] = []
    for k in sorted(votes):
        year, state, st, county, fips, office, district, cand, party, mode = k
        out.append(
            dict(
                year=year,
                state=state,
                state_po=st,
                county_name=county,
                county_fips=fips,
                office=office,
                district=district,
                candidate=cand,
                party=party,
                mode=mode,
                candidatevotes=votes[k],
                totalvotes=totals[(fips, office, district)],
            )
        )
    return out


def _csv_rows_from_zip(data: bytes) -> Iterator[dict[str, str]]:
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        for name in z.namelist():
            if name.lower().endswith(".csv") and not name.startswith("__MACOSX"):
                with z.open(name) as f:
                    yield from csv.DictReader(io.TextIOWrapper(f, encoding="utf-8", errors="replace"))


def download_state(
    year: int, state: str, client: httpx.Client, *, house: bool = False, house_only: bool = False
) -> list[dict[str, str | int]]:
    """Fetch one state's precinct zip and aggregate it."""
    url = URLS[year].format(st=state.lower())
    resp = client.get(url, follow_redirects=True, timeout=300)
    resp.raise_for_status()
    return aggregate_precincts(_csv_rows_from_zip(resp.content), house=house, house_only=house_only)


Fetcher = Callable[[int, str, httpx.Client], list[dict[str, str | int]]]


@dataclass
class DownloadResult:
    out: Path
    done: list[str] = field(default_factory=list)  # states written (fresh or cached)
    cached: list[str] = field(default_factory=list)
    failed: dict[str, str] = field(default_factory=dict)  # state -> error
    empty: list[str] = field(default_factory=list)  # fetched, but no Senate/Governor/President rows


def cache_path(out: Path, year: int, state: str, *, house: bool = False, house_only: bool = False) -> Path:
    variant = ".house" if house_only else ".with-house" if house else ""
    return out.parent / "medsl" / str(year) / f"{state.upper()}{variant}.csv"


def _write_rows(path: Path, rows: Iterable[dict[str, str | int]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    with tmp.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=HEADER)
        w.writeheader()
        w.writerows(rows)
    tmp.replace(path)


def _read_rows(path: Path) -> list[dict[str, str | int]]:
    with path.open(newline="") as f:
        return [dict(r) for r in csv.DictReader(f)]


def download(
    year: int,
    states: Iterable[str],
    out: Path,
    *,
    house: bool = False,
    house_only: bool = False,
    refresh: bool = False,
    fetch: Fetcher | None = None,
) -> DownloadResult:
    """Download and aggregate `states` for `year` into one CSV at `out`.

    Per-state results are cached next to `out` (`medsl/<year>/<ST>.csv`) and reused unless
    `refresh`. A state that fails to download is logged and recorded in `DownloadResult.failed`;
    the merged CSV still contains every state that succeeded (or was cached). A state whose file
    has no kept office that year lands in `DownloadResult.empty` (and is fetched again next time).
    """
    result = DownloadResult(out)
    merged: list[dict[str, str | int]] = []
    fetcher: Fetcher = fetch or partial(download_state, house=house, house_only=house_only)
    with httpx.Client(headers={"User-Agent": "susquatch-replay/0.1"}) as client:
        for st in [s.upper() for s in states]:
            cached = cache_path(out, year, st, house=house, house_only=house_only)
            if cached.exists() and not refresh:
                merged.extend(_read_rows(cached))
                result.done.append(st)
                result.cached.append(st)
                continue
            try:
                rows = fetcher(year, st, client)
            except Exception as exc:  # one bad state must not sink the batch
                log.warning("%s %s: download failed: %s", year, st, exc)
                result.failed[st] = f"{type(exc).__name__}: {exc}"
                continue
            if not rows:  # e.g. DE/NJ/VA had no Senate or Governor race in 2022: not an error
                log.info("%s %s: no Senate/Governor/President rows", year, st)
                result.empty.append(st)
                continue
            _write_rows(cached, rows)
            merged.extend(rows)
            result.done.append(st)
    out.parent.mkdir(parents=True, exist_ok=True)
    _write_rows(out, merged)
    return result


__all__ = [
    "HEADER",
    "OFFICES",
    "DownloadResult",
    "aggregate_precincts",
    "download",
    "download_state",
    "normalize_district",
]
