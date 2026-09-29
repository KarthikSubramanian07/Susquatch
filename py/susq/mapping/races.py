"""Deterministic race and party parsing shared by the Cup mapping and both venue normalizers.

A single parser for every venue is the point: the Cup title "Will the Democratic Party win the
Ohio Senate?", Kalshi's "Will Democratics win the Senate race in Ohio?" and Polymarket's
"Will the Democrats win the Ohio Senate race in 2026?" all land on the same `2026-SEN-OH-S` key,
so joining venues is an exact key match rather than fuzzy string similarity.

Race-id grammar::

    2026-SEN-NH          Senate seat (regular)
    2026-SEN-OH-S        Senate special (only OH and FL in 2026)
    2026-GOV-GA          Governor
    2026-HOUSE-NY-17     House district (two-digit), at-large = "AL"
    2026-CONTROL-SENATE  chamber control (kind "chamber")
    2026-CONTROL-HOUSE
"""

from __future__ import annotations

import re
from dataclasses import dataclass

from susq.mapping.geo import CYCLE, SENATE_SPECIALS, STATE_BY_NAME, STATE_NAMES

DEM, REP, IND = "DEM", "REP", "IND"

# Anything that looks like a winner market but settles on something else.
_REJECT = re.compile(
    r"margin|\bby \d|within \d|turnout|votes cast|closest|\bseats\b|\band will\b|&|"
    r"popular vote|primary|nominee|first round|speaker|leader|trifecta|supermajority|\bhold\b|"
    r"\bany\b|recount|endorse|20(?:27|28|30)|\bcandidate\b",
    re.IGNORECASE,
)
_CHAMBER = re.compile(
    r"\b(?:win|control|take)\b(?: control of)? the (?:u\.?s\.? )?(senate|house)(?: of representatives)?"
    r"(?: in 2026| after the 2026 midterm elections?)?\s*\??\s*$",
    re.IGNORECASE,
)
_DISTRICT_CODE = re.compile(r"\b([A-Z]{2})-(\d{1,2}|AL)\b")
_DISTRICT_ORDINAL = re.compile(r"(\d{1,2})(?:st|nd|rd|th)(?: congressional)? district", re.IGNORECASE)
_STATE_NAME = re.compile(
    r"\b(" + "|".join(sorted((re.escape(n) for n in STATE_BY_NAME), key=len, reverse=True)) + r")\b",
    re.IGNORECASE,
)
_SENATE = re.compile(r"\bsenat", re.IGNORECASE)
_GOVERNOR = re.compile(r"\bgovern|gubernatorial", re.IGNORECASE)
_HOUSE = re.compile(r"\bhouse\b|\bcongressional\b", re.IGNORECASE)
_PARTY_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    (IND, re.compile(r"\bindependent\b", re.IGNORECASE)),
    (DEM, re.compile(r"\bdemocrat(?:s|ic|ics)?\b", re.IGNORECASE)),
    (REP, re.compile(r"\brepublicans?\b|\bgop\b", re.IGNORECASE)),
)


@dataclass(frozen=True)
class RaceKey:
    """A normalized race. `kind` is "race_party" for seats and "chamber" for control markets."""

    race_id: str
    kind: str
    office: str  # SEN | GOV | HOUSE | CONTROL
    state: str = ""  # "" for chamber markets
    district: str = ""  # "17", "AL" or ""
    special: bool = False


def senate(state: str) -> RaceKey:
    special = state in SENATE_SPECIALS
    return RaceKey(f"{CYCLE}-SEN-{state}{'-S' if special else ''}", "race_party", "SEN", state, "", special)


def governor(state: str) -> RaceKey:
    return RaceKey(f"{CYCLE}-GOV-{state}", "race_party", "GOV", state)


def house_seat(state: str, district: str) -> RaceKey:
    d = "AL" if district.upper() in {"AL", "0", "00"} else f"{int(district):02d}"
    return RaceKey(f"{CYCLE}-HOUSE-{state}-{d}", "race_party", "HOUSE", state, d)


def chamber(which: str) -> RaceKey:
    name = which.upper()
    if name not in {"SENATE", "HOUSE"}:
        raise ValueError(f"unknown chamber {which!r}")
    return RaceKey(f"{CYCLE}-CONTROL-{name}", "chamber", "CONTROL")


def race_from_id(race_id: str) -> RaceKey:
    """Inverse of the race-id grammar (raises ValueError on anything else)."""
    parts = race_id.split("-")
    try:
        if parts[0] != str(CYCLE):
            raise ValueError
        office = parts[1]
        if office == "CONTROL" and len(parts) == 3:
            return chamber(parts[2])
        if office == "SEN" and len(parts) in (3, 4):
            key = senate(parts[2])
            if key.race_id != race_id:
                raise ValueError
            return key
        if office == "GOV" and len(parts) == 3:
            return governor(parts[2])
        if office == "HOUSE" and len(parts) == 4:
            return house_seat(parts[2], parts[3])
    except (IndexError, ValueError):
        pass
    raise ValueError(f"not a race id: {race_id!r}")


def parse_party(text: str) -> str | None:
    """Party whose win resolves YES, or None if the text names zero or several parties."""
    found = {party for party, pat in _PARTY_PATTERNS if pat.search(text)}
    if IND in found:
        return IND  # "Will Dan Osborn (as an independent) win" names no other party
    return found.pop() if len(found) == 1 else None


def is_rejected(text: str) -> bool:
    """True for markets that are not plain "party X wins race Y" winners (margins, combos, ...)."""
    return bool(_REJECT.search(text))


def parse_race(text: str) -> RaceKey | None:
    """Map a market title/question to a race, or None when it is not an unambiguous winner market."""
    if is_rejected(text):
        return None
    if m := _CHAMBER.search(text):
        states = {s.lower() for s in _STATE_NAME.findall(text)}
        return None if states else chamber(m.group(1))

    if m := _DISTRICT_CODE.search(text):
        state, district = m.group(1), m.group(2)
        return house_seat(state, district) if state in STATE_NAMES else None

    states = {STATE_BY_NAME[s.lower()] for s in _STATE_NAME.findall(text)}
    if len(states) != 1:
        return None
    (state,) = states
    offices = [o for o, pat in (("SEN", _SENATE), ("GOV", _GOVERNOR), ("HOUSE", _HOUSE)) if pat.search(text)]
    if len(offices) != 1:
        return None
    office = offices[0]
    if office == "SEN":
        return senate(state)
    if office == "GOV":
        return governor(state)
    if m := _DISTRICT_ORDINAL.search(text):
        return house_seat(state, m.group(1))
    if re.search(r"at[- ]large", text, re.IGNORECASE):
        return house_seat(state, "AL")
    # "Alaska House winner?" (single-district states) — only safe when the title has nothing else.
    if re.fullmatch(rf"{STATE_NAMES[state]} House winner\??", text.strip(), re.IGNORECASE):
        return house_seat(state, "AL")
    return None
