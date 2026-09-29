"""Per-state election-night reporting conventions used by the synthetic replay builder and as the
`typical_report_order` in needle priors.

These are **modeling assumptions**, not measurements. They encode the broad, well-known shape of
recent (2020-2024) nights: states that pre-process mail and early votes publish them first, in the
first hour after polls close, then Election Day precincts trickle in overnight, and provisional /
late-arriving mail comes days later. Pennsylvania and Wisconsin cannot pre-process mail, so mail is
counted late on the night. California counts mail received before Election Day first, E-day
overnight, and a large share of late mail over the following weeks. Every number here should be
recalibrated from real captures (`enr-capture` during CA SoS test windows and on the night);
`docs/ELECTION_NIGHT.md` tracks which are still assumptions.

Times are minutes after the state's last poll close, for a median-size county; the builder scales
them by county size and adds log-normal noise.
"""

from __future__ import annotations

from dataclasses import dataclass

from susq.replay.history import EARLY, EDAY, MAIL, MIXED, PROVISIONAL, VoteType


@dataclass(frozen=True)
class TypeConvention:
    """How one vote type reports on election night."""

    first_report_min: float  # median minutes after poll close of the first batch
    spread_min: float  # median minutes between later batches
    batches: tuple[int, int]  # inclusive range of batches on the night for a large county
    night_completion: float  # share of this type's final votes counted on the night
    report_order: float  # 0 = reports first ... 1 = reports last (NeedlePrior.typical_report_order)


@dataclass(frozen=True)
class StateConvention:
    name: str
    types: dict[VoteType, TypeConvention]
    note: str

    def for_type(self, vt: VoteType) -> TypeConvention:
        return self.types.get(vt, self.types[MIXED])


_PREPROCESSED_FIRST = StateConvention(
    name="mail_early_first",
    note="Mail and early votes pre-processed and released right after close; E-day overnight.",
    types={
        MAIL: TypeConvention(12, 45, (1, 3), 0.97, 0.05),
        EARLY: TypeConvention(15, 40, (1, 3), 1.0, 0.10),
        EDAY: TypeConvention(45, 35, (2, 6), 1.0, 0.60),
        PROVISIONAL: TypeConvention(600, 240, (1, 1), 0.25, 1.0),
        MIXED: TypeConvention(30, 45, (2, 6), 0.98, 0.50),
    },
)

_MAIL_LATE = StateConvention(
    name="eday_first_mail_late",
    note="No mail pre-processing: E-day and early in-person first, mail counted late into the night.",
    types={
        EARLY: TypeConvention(20, 40, (1, 3), 1.0, 0.10),
        EDAY: TypeConvention(40, 35, (2, 6), 1.0, 0.35),
        MAIL: TypeConvention(180, 120, (2, 5), 0.85, 0.80),
        PROVISIONAL: TypeConvention(900, 300, (1, 1), 0.2, 1.0),
        MIXED: TypeConvention(40, 60, (2, 6), 0.95, 0.50),
    },
)

_CALIFORNIA = StateConvention(
    name="california",
    note="Pre-Election-Day mail at ~8pm PT, E-day overnight, large late-mail tail over weeks.",
    types={
        MAIL: TypeConvention(8, 90, (1, 3), 0.60, 0.40),
        EARLY: TypeConvention(15, 60, (1, 2), 0.95, 0.20),
        EDAY: TypeConvention(60, 60, (2, 5), 0.95, 0.50),
        PROVISIONAL: TypeConvention(2000, 1000, (1, 1), 0.0, 1.0),
        MIXED: TypeConvention(15, 90, (2, 5), 0.60, 0.50),
    },
)

_DEFAULT = StateConvention(
    name="default",
    note="Unknown state: county totals in several batches through the night.",
    types={
        MAIL: TypeConvention(20, 60, (1, 3), 0.95, 0.20),
        EARLY: TypeConvention(20, 50, (1, 3), 1.0, 0.20),
        EDAY: TypeConvention(45, 45, (2, 5), 1.0, 0.60),
        PROVISIONAL: TypeConvention(900, 300, (1, 1), 0.2, 1.0),
        MIXED: TypeConvention(30, 50, (2, 5), 0.97, 0.50),
    },
)

_BY_STATE: dict[str, StateConvention] = {
    **{
        s: _PREPROCESSED_FIRST
        for s in ("GA", "NC", "FL", "AZ", "OH", "TX", "IA", "SC", "KS", "NE", "NV", "NH", "ME", "MI", "AK")
    },
    **{s: _MAIL_LATE for s in ("PA", "WI")},
    "CA": _CALIFORNIA,
}


def convention_for(state: str) -> StateConvention:
    """Reporting convention for a USPS state code (default when not modeled)."""
    return _BY_STATE.get(state.upper(), _DEFAULT)


__all__ = [
    "EARLY",
    "EDAY",
    "MAIL",
    "MIXED",
    "PROVISIONAL",
    "StateConvention",
    "TypeConvention",
    "convention_for",
]
