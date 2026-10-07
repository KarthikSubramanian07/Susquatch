"""Historical county results (MEDSL-style CSV) → per-county, per-vote-type vote counts.

Input schema (one row per county × candidate × mode), as produced by `download.py` and shipped in
the test fixture:

    year,state,state_po,county_name,county_fips,office,district,candidate,party,mode,candidatevotes,totalvotes

`party` is MEDSL's simplified party (DEMOCRAT / REPUBLICAN / LIBERTARIAN / OTHER). `mode` keeps the
state's own labels (ABSENTEE, ADVANCED, ELECTION DAY, PROVISIONAL, ONE STOP, TOTAL, ...). Some
states publish a TOTAL row *alongside* the split rows, so TOTAL is dropped for any county that also
has split rows (otherwise every vote would count twice).
"""

from __future__ import annotations

import csv
from collections import defaultdict
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path

from susq.contracts import bus_pb2

VoteType = int  # bus_pb2.VoteType value

MAIL = bus_pb2.VOTE_TYPE_MAIL
EARLY = bus_pb2.VOTE_TYPE_EARLY_IN_PERSON
EDAY = bus_pb2.VOTE_TYPE_ELECTION_DAY
PROVISIONAL = bus_pb2.VOTE_TYPE_PROVISIONAL
MIXED = bus_pb2.VOTE_TYPE_MIXED
SPLIT_TYPES: tuple[VoteType, ...] = (MAIL, EARLY, EDAY, PROVISIONAL)


def classify_mode(mode: str) -> VoteType:
    """Map a MEDSL/state vote-mode label to a contract `VoteType` (same rules as the Rust poller)."""
    m = mode.strip().lower()
    if "provi" in m:
        return PROVISIONAL
    if any(k in m for k in ("mail", "absentee", "vbm", "postal")):
        return MAIL
    if any(k in m for k in ("advance", "early", "one stop")):
        return EARLY
    if any(k in m for k in ("election day", "in-person", "in person", "precinct", "failsafe", "machine")):
        return EDAY
    return MIXED


INDEPENDENT = "INDEPENDENT"  # the party label `pool_independent` gives the pooled candidate


@dataclass
class Tally:
    """Two-party-aware vote tally."""

    dem: int = 0
    rep: int = 0
    other: int = 0

    @property
    def total(self) -> int:
        return self.dem + self.rep + self.other

    @property
    def two_party(self) -> int:
        return self.dem + self.rep

    def margin_pts(self) -> float:
        """D − R as percentage points of the two-party vote (0 when there are no two-party votes)."""
        tp = self.two_party
        return 100.0 * (self.dem - self.rep) / tp if tp else 0.0

    def add(self, party: str, votes: int) -> None:
        if party == "DEMOCRAT":
            self.dem += votes
        elif party == "REPUBLICAN":
            self.rep += votes
        else:
            self.other += votes

    def __iadd__(self, o: Tally) -> Tally:
        self.dem += o.dem
        self.rep += o.rep
        self.other += o.other
        return self


@dataclass
class CountyHistory:
    """One county's result in one historical contest, by vote type and by candidate."""

    state: str
    county_fips: str
    county_name: str
    by_type: dict[VoteType, Tally] = field(default_factory=dict)
    # candidate -> (party, {vote_type: votes}); keeps real names for synthetic replays.
    candidates: dict[str, tuple[str, dict[VoteType, int]]] = field(default_factory=dict)

    @property
    def tally(self) -> Tally:
        t = Tally()
        for v in self.by_type.values():
            t += v
        return t

    @property
    def is_split(self) -> bool:
        return any(vt != MIXED for vt in self.by_type)


@dataclass
class ContestHistory:
    """A statewide contest: every county's history plus identifying metadata."""

    year: int
    state: str
    office: str
    counties: dict[str, CountyHistory]

    @property
    def tally(self) -> Tally:
        t = Tally()
        for c in self.counties.values():
            t += c.tally
        return t


_PARTY_CODE = {"DEMOCRAT": "DEM", "REPUBLICAN": "REP", "LIBERTARIAN": "LIB", INDEPENDENT: "IND"}


def party_code(medsl_party: str) -> str:
    """MEDSL simplified party → contract party code."""
    return _PARTY_CODE.get(medsl_party.strip().upper(), "OTH")


HistoryPaths = Path | Sequence[Path]


def _paths(path: HistoryPaths) -> list[Path]:
    return [path] if isinstance(path, Path) else list(path)


@dataclass(frozen=True)
class ContestKey:
    year: int
    state: str
    office: str
    district: str = "STATEWIDE"


def _contest_from_rows(key: ContestKey, rows: list[dict[str, str]]) -> ContestHistory:
    split_counties = {r["county_fips"] for r in rows if r["mode"].strip().upper() != "TOTAL"}
    counties: dict[str, CountyHistory] = {}
    for r in rows:
        fips = r["county_fips"].strip().zfill(5)
        mode = r["mode"].strip().upper()
        if mode == "TOTAL" and r["county_fips"] in split_counties:
            continue
        vt = MIXED if mode == "TOTAL" else classify_mode(mode)
        votes = int(float(r["candidatevotes"] or 0))
        party = r["party"].strip().upper()
        c = counties.setdefault(fips, CountyHistory(key.state, fips, r["county_name"].strip()))
        c.by_type.setdefault(vt, Tally()).add(party, votes)
        cand = r["candidate"].strip()
        _, per_type = c.candidates.setdefault(cand, (party, defaultdict(int)))
        per_type[vt] += votes
    for c in counties.values():
        c.candidates = {k: (p, dict(v)) for k, (p, v) in c.candidates.items()}
    return ContestHistory(year=key.year, state=key.state, office=key.office, counties=counties)


def _row_key(row: dict[str, str]) -> ContestKey:
    district = (row.get("district") or "STATEWIDE").strip().upper() or "STATEWIDE"
    return ContestKey(int(row["year"]), row["state_po"].upper(), row["office"].upper(), district)


def load_contest(
    path: HistoryPaths, *, year: int, state: str, office: str, district: str = "STATEWIDE"
) -> ContestHistory:
    """Load one contest (e.g. 2022 GA US SENATE) from one or more county results CSVs.

    `district` selects a US House contest from a CSV built with `download --house`; statewide
    offices use the default. Rows with a blank district column count as statewide.
    """
    want = ContestKey(year, state.upper(), office.upper(), district.upper())
    rows: list[dict[str, str]] = []
    for p in _paths(path):
        with p.open(newline="") as f:
            rows.extend(row for row in csv.DictReader(f) if _row_key(row) == want)
    if not rows:
        raise ValueError(f"no rows for {year} {state} {office} {district} in {_paths(path)}")
    return _contest_from_rows(want, rows)


def load_all(path: HistoryPaths) -> dict[ContestKey, ContestHistory]:
    """Every contest in the CSVs, in one pass (the spec builder needs totals for every state)."""
    grouped: dict[ContestKey, list[dict[str, str]]] = defaultdict(list)
    for p in _paths(path):
        with p.open(newline="") as f:
            for row in csv.DictReader(f):
                grouped[_row_key(row)].append(row)
    return {k: _contest_from_rows(k, rows) for k, rows in grouped.items()}


def top_independent(contest: ContestHistory) -> tuple[str, float] | None:
    """The leading non-D/R/L candidate statewide and their share of all votes (None if there is none)."""
    votes: dict[str, int] = defaultdict(int)
    total = 0
    for c in contest.counties.values():
        for cand, (party, per_type) in c.candidates.items():
            n = sum(per_type.values())
            total += n
            if party not in ("DEMOCRAT", "REPUBLICAN", "LIBERTARIAN"):
                votes[cand] += n
    if not votes or not total:
        return None
    cand = max(votes, key=lambda k: votes[k])
    return cand, votes[cand] / total


def pool_independent(contest: ContestHistory) -> ContestHistory:
    """Count the leading independent with the Democrat, as the needle does for `independent_major` races.

    The needle's "side A" in an `independent_major` race is DEM + IND votes (MODELS §3.3), so the
    historical baseline has to measure the same thing: the leading non-D/R/L candidate (Osborn in
    2024 NE Senate) is moved from `other` to `dem` in every tally and relabelled `INDEPENDENT`
    (contract party `IND`) so synthetic replays report them as an independent. A contest without
    such a candidate is returned unchanged.
    """
    top = top_independent(contest)
    if top is None:
        return contest
    ind = top[0]
    counties: dict[str, CountyHistory] = {}
    for fips, c in contest.counties.items():
        by_type = {vt: Tally(t.dem, t.rep, t.other) for vt, t in c.by_type.items()}
        candidates = dict(c.candidates)
        if ind in candidates:
            _, per_type = candidates[ind]
            for vt, n in per_type.items():
                t = by_type.setdefault(vt, Tally())
                t.other -= n
                t.dem += n
            candidates[ind] = (INDEPENDENT, dict(per_type))
        counties[fips] = CountyHistory(c.state, c.county_fips, c.county_name, by_type, candidates)
    return ContestHistory(contest.year, contest.state, contest.office, counties)
