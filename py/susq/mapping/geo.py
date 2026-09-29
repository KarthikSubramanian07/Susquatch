"""Static political geography for the 2026 cycle: states, regions, named clusters, Senate seats.

Every table here is public knowledge, typed in once and covered by tests. When reality disagrees
(a special election appears, a poll-closing time is confirmed), fix it here and nowhere else.
"""

from __future__ import annotations

from types import MappingProxyType

CYCLE = 2026

STATE_NAMES: MappingProxyType[str, str] = MappingProxyType(
    {
        "AL": "Alabama", "AK": "Alaska", "AZ": "Arizona", "AR": "Arkansas", "CA": "California",
        "CO": "Colorado", "CT": "Connecticut", "DE": "Delaware", "FL": "Florida", "GA": "Georgia",
        "HI": "Hawaii", "ID": "Idaho", "IL": "Illinois", "IN": "Indiana", "IA": "Iowa",
        "KS": "Kansas", "KY": "Kentucky", "LA": "Louisiana", "ME": "Maine", "MD": "Maryland",
        "MA": "Massachusetts", "MI": "Michigan", "MN": "Minnesota", "MS": "Mississippi",
        "MO": "Missouri", "MT": "Montana", "NE": "Nebraska", "NV": "Nevada", "NH": "New Hampshire",
        "NJ": "New Jersey", "NM": "New Mexico", "NY": "New York", "NC": "North Carolina",
        "ND": "North Dakota", "OH": "Ohio", "OK": "Oklahoma", "OR": "Oregon", "PA": "Pennsylvania",
        "RI": "Rhode Island", "SC": "South Carolina", "SD": "South Dakota", "TN": "Tennessee",
        "TX": "Texas", "UT": "Utah", "VT": "Vermont", "VA": "Virginia", "WA": "Washington",
        "WV": "West Virginia", "WI": "Wisconsin", "WY": "Wyoming",
    }
)  # fmt: skip

STATE_BY_NAME: MappingProxyType[str, str] = MappingProxyType({v.lower(): k for k, v in STATE_NAMES.items()})

# US Census regions, with the West standing in for SPEC §5's "Mountain-West".
REGIONS: MappingProxyType[str, frozenset[str]] = MappingProxyType(
    {
        "northeast": frozenset({"CT", "ME", "MA", "NH", "RI", "VT", "NJ", "NY", "PA"}),
        "midwest": frozenset({"IL", "IN", "MI", "OH", "WI", "IA", "KS", "MN", "MO", "NE", "ND", "SD"}),
        "south": frozenset(
            {"DE", "FL", "GA", "MD", "NC", "SC", "VA", "WV", "AL", "KY", "MS", "TN", "AR", "LA", "OK", "TX"}
        ),
        "west": frozenset({"AZ", "CO", "ID", "MT", "NV", "NM", "UT", "WY", "AK", "CA", "HI", "OR", "WA"}),
    }
)

# Named clusters from SPEC §5 / RESEARCH §5. "house" is attached to every House contract.
NAMED_CLUSTERS: MappingProxyType[str, frozenset[str]] = MappingProxyType(
    {
        "red-state-reach": frozenset({"TX", "OH", "IA", "KS", "AK", "SC"}),
        "new-england-incumbency": frozenset({"ME"}),
        "midwest-industrial": frozenset({"MI", "OH"}),
    }
)

# 2026 Senate map: 33 Class-2 seats plus the OH and FL specials (Vance and Rubio seats).
# Value = party currently holding the seat. Chamber today: 53 R - 47 D (incl. independents caucusing D).
SENATE_SEATS_UP: MappingProxyType[str, str] = MappingProxyType(
    {
        **dict.fromkeys(
            ["CO", "DE", "GA", "IL", "MA", "MI", "MN", "NH", "NJ", "NM", "OR", "RI", "VA"], "DEM"
        ),
        **dict.fromkeys(
            ["AL", "AK", "AR", "ID", "IA", "KS", "KY", "LA", "ME", "MS", "MT", "NE", "NC", "OK",
             "SC", "SD", "TN", "TX", "WV", "WY", "OH", "FL"],
            "REP",
        ),
    }
)  # fmt: skip
SENATE_SPECIALS: frozenset[str] = frozenset({"OH", "FL"})
SENATE_TOTAL_D, SENATE_TOTAL_R = 47, 53
SENATE_D_NOT_UP = SENATE_TOTAL_D - sum(1 for p in SENATE_SEATS_UP.values() if p == "DEM")
SENATE_R_NOT_UP = SENATE_TOTAL_R - sum(1 for p in SENATE_SEATS_UP.values() if p == "REP")
SENATE_D_CONTROL_SEATS = 51  # 50-50 goes to the Republican VP
HOUSE_SEATS, HOUSE_CONTROL_SEATS = 435, 218

RCV_STATES: frozenset[str] = frozenset({"AK", "ME"})
RUNOFF_STATES: frozenset[str] = frozenset({"GA", "LA"})  # LA's 2026 system is unverified (RESEARCH §8)

# Approximate statewide poll-closing hour, ET (24h; >24 = after midnight). RESEARCH §7 marks these
# "confirm with each SoS before Oct 15". Used only to tag early-reporting legs as rolling fuel.
POLL_CLOSE_ET: MappingProxyType[str, float] = MappingProxyType(
    {
        **dict.fromkeys(["GA", "SC", "VA", "VT", "IN", "KY"], 19.0),
        **dict.fromkeys(["NC", "OH", "WV"], 19.5),
        **dict.fromkeys(
            ["ME", "NH", "PA", "MI", "FL", "TX", "KS", "MA", "NJ", "DE", "MD", "CT", "IL", "AL",
             "MS", "MO", "OK", "TN", "RI"],
            20.0,
        ),
        **dict.fromkeys(["AZ", "IA", "NE", "CO", "MN", "WI", "NM", "NY", "LA", "SD", "ND", "WY", "AR"], 21.0),
        **dict.fromkeys(["NV", "UT", "MT", "ID"], 22.0),
        **dict.fromkeys(["CA", "OR", "WA"], 23.0),
        "HI": 24.0,
        "AK": 25.0,
    }
)  # fmt: skip
EARLY_REPORTING_CLOSE_ET = 20.0


def region_of(state: str) -> str:
    """Census region name for a two-letter state code."""
    for name, states in REGIONS.items():
        if state in states:
            return name
    raise KeyError(f"unknown state {state!r}")


def clusters_for(state: str, *, house: bool) -> list[str]:
    """Cluster tags for a contract in `state` (the vocabulary used by risk and target files)."""
    tags = ["national", f"region:{region_of(state)}", f"state:{state}"]
    tags += [name for name, states in NAMED_CLUSTERS.items() if state in states]
    if house:
        tags.append("house")
    return tags


def is_early_reporting(state: str) -> bool:
    return POLL_CLOSE_ET.get(state, 99.0) <= EARLY_REPORTING_CLOSE_ET
