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


_PARTY_CODE = {"DEMOCRAT": "DEM", "REPUBLICAN": "REP", "LIBERTARIAN": "LIB"}


def party_code(medsl_party: str) -> str:
    """MEDSL simplified party → contract party code."""
    return _PARTY_CODE.get(medsl_party.strip().upper(), "OTH")


def load_contest(path: Path, *, year: int, state: str, office: str) -> ContestHistory:
    """Load one contest (e.g. 2022 GA US SENATE) from a county results CSV."""
    rows: list[dict[str, str]] = []
    with path.open(newline="") as f:
        for row in csv.DictReader(f):
            if (
                row["year"] == str(year)
                and row["state_po"].upper() == state
                and row["office"].upper() == office.upper()
            ):
                rows.append(row)
    if not rows:
        raise ValueError(f"no rows for {year} {state} {office} in {path}")

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
        c = counties.setdefault(fips, CountyHistory(state, fips, r["county_name"].strip()))
        c.by_type.setdefault(vt, Tally()).add(party, votes)
        cand = r["candidate"].strip()
        _, per_type = c.candidates.setdefault(cand, (party, defaultdict(int)))
        per_type[vt] += votes
    for c in counties.values():
        c.candidates = {k: (p, dict(v)) for k, (p, v) in c.candidates.items()}
    return ContestHistory(year=year, state=state, office=office.upper(), counties=counties)
