"""The 2026 needle prior spec: every Cup Senate and Governor race → `specs/general_2026.toml`.

`build_spec` turns three inputs into a spec that `spec.load_spec` reads:

* **Races** come from the market map (`state/market_map.json`): one race per `race_party` row whose
  race id is a Senate or Governor race (`2026-SEN-GA`, `2026-GOV-CA`, `2026-SEN-FL-S`).
* **Prior margin** comes from the fair file (`state/fair_live.json`, a `TargetFile`). The race's
  left side is the Democrat, or the independent where the independent out-prices the Democrat (the
  rule in `calibrate.consensus.left_party`; Nebraska Senate). `p_left = p_left_yes / (p_left_yes +
  p_rep_yes)` removes the two-way overround, like `left_target`. The pre-election sim maps margins to
  win probabilities with the race's unconditional margin SD (`models.numpy_engine.
  analytic_margin_sd`, the same `RaceSpec` the calibration job builds), and the needle reads
  `prior_margin_sd` as the race's *total* prior SD (it subtracts the national and county parts
  itself, MODELS §3.3). So the conversion is::

      prior_margin_sd   = sd = analytic_margin_sd(race spec, SimParams())     # about 5.1 pts
      prior_margin_mean = sd * Phi^-1(p_left)

  and the needle's 0%-reporting probability `Phi(mean / sd)` equals the fair value. A price beyond
  95/5 says "safe" but not "how safe": 0.99 maps to only 2.3 SD (about 12 pts), while a Wyoming
  race is 40+ pts. Such a race would drag the needle's shared national shift as soon as it reports,
  so for a *saturated* price the historical margin is used instead when it points the same way and
  is larger (recorded per race).
* **History** is the MEDSL county CSVs from `download` (2022 and 2024). The baseline contest is the
  same-state statewide race closest to a 2026 midterm: 2022 same office, then 2022 other office, then
  2024 same office, then 2024 President. A contest is usable only if each side has at least 15% of
  the two-party vote (Alaska 2022 Senate was R vs R; New Mexico 2022 has no party labels at all).
  Independent-led races pool the leading independent with the Democrat (`history.pool_independent`)
  and prefer a contest where an independent ran strongly, then any same-office contest (MEDSL labels
  Osborn, the 2024 NE Senate independent, DEMOCRAT, so that contest already measures D + IND).
* **Turnout ratio** scales the baseline to a midterm: 1.0 for a 2022 baseline; for a 2024 baseline
  it is the state's 2022 statewide total over its 2024 total, or the median of that ratio over the
  states that have both years when the state has no 2022 contest.
* **Poll close** is the state's last close from `config/poll_closing.toml`, pinned per race.

House races are not emitted. The needle keys county priors by (race, county FIPS), and most
competitive districts split counties (CA-22, CA-48), so a district cannot be modelled from county
history without a county x district key in the ENR feed and the prior. `download --house` keeps the
district rows for when that exists; until then House races are listed as uncovered.
"""

from __future__ import annotations

import math
import statistics
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path

from susq.calibrate.job import _spec_for
from susq.contracts import from_json, ops_pb2, targets_pb2
from susq.mapping.races import DEM, IND, REP, race_from_id
from susq.models.numpy_engine import analytic_margin_sd
from susq.models.types import SimParams
from susq.replay.closing import DEFAULT_PATH as POLL_CLOSING_PATH
from susq.replay.closing import load_closings
from susq.replay.history import ContestHistory, ContestKey, load_all, pool_independent, top_independent

SEN, GOV, PRES = "US SENATE", "GOVERNOR", "US PRESIDENT"
STATEWIDE_OFFICES = {"SEN": SEN, "GOV": GOV}
BASELINES: dict[str, tuple[tuple[int, str], ...]] = {
    "SEN": ((2022, SEN), (2022, GOV), (2024, SEN), (2024, PRES)),
    "GOV": ((2022, GOV), (2022, SEN), (2024, GOV), (2024, PRES)),
}
MIDTERM_YEAR = 2022
MIN_SIDE_SHARE = 0.15  # each side's share of the two-party vote for a usable baseline
STRONG_IND_SHARE = 0.20  # an independent-led race prefers a baseline where an independent got this
SATURATED_Z = 1.645  # |Phi^-1(p)| at 95/5
P_CLIP = 0.001
HOUSE_REASON = "House: needle priors are keyed by county FIPS and the district splits counties"


def _office(race_id: str) -> str:
    parts = race_id.split("-")
    return parts[1] if len(parts) >= 3 else ""


def _state(race_id: str) -> str:
    return race_id.split("-")[2]


def _load_map(path: Path) -> ops_pb2.MarketMap:
    return from_json(ops_pb2.MarketMap, path.read_text())


def cup_states(market_map_path: Path) -> set[str]:
    """USPS codes of every state with a Senate or Governor `race_party` row in the market map."""
    mm = _load_map(market_map_path)
    return {
        _state(m.race_id)
        for m in mm.markets
        if m.kind == "race_party" and _office(m.race_id) in STATEWIDE_OFFICES
    }


@dataclass(frozen=True)
class RaceRow:
    """One emitted `[[races]]` entry plus the provenance written next to it."""

    race_id: str
    state: str
    year: int
    office: str
    left_party: str
    p_left: float
    prior_margin_mean: float
    prior_margin_sd: float
    turnout_ratio: float
    poll_close_utc: str
    national_beta: float = 1.0
    independent_major: bool = False
    notes: tuple[str, ...] = ()


@dataclass(frozen=True)
class Uncovered:
    race_id: str
    reason: str


@dataclass
class GeneratedSpec:
    history: tuple[str, ...]
    national_shift_prior_sd: float
    races: list[RaceRow] = field(default_factory=list)
    uncovered: list[Uncovered] = field(default_factory=list)
    history_states: dict[int, set[str]] = field(default_factory=dict)  # year -> states with SEN/GOV/PRES


def margin_from_p(p_left: float, sd: float) -> float:
    """The sim's margin → probability map inverted: `p = Phi(m / sd)` ⇒ `m = sd * Phi^-1(p)`."""
    p = min(max(p_left, P_CLIP), 1.0 - P_CLIP)
    return sd * statistics.NormalDist().inv_cdf(p)


def _left_and_p(parties: dict[str, float]) -> tuple[str, float] | None:
    """(left party, P(left beats R)) from fair `p_yes` by party, as `consensus.left_party/left_target`."""
    ind, dem, rep = parties.get(IND), parties.get(DEM), parties.get(REP)
    left = IND if ind is not None and (dem is None or ind > dem) else DEM
    pl = parties.get(left)
    if pl is not None and rep is not None and pl + rep > 0:
        return left, pl / (pl + rep)
    if pl is not None:
        return left, pl
    if rep is not None:
        return left, 1.0 - rep
    return None


def _usable(c: ContestHistory) -> bool:
    t = c.tally
    return t.two_party > 0 and min(t.dem, t.rep) / t.two_party >= MIN_SIDE_SHARE


def _baseline(
    contests: dict[ContestKey, ContestHistory], office: str, state: str, independent: bool
) -> ContestHistory | None:
    order = list(BASELINES[office])
    if independent:
        same = STATEWIDE_OFFICES[office]

        def rank(yo: tuple[int, str]) -> int:
            c = contests.get(ContestKey(yo[0], state, yo[1]))
            top = top_independent(c) if c is not None else None
            if top is not None and top[1] >= STRONG_IND_SHARE:
                return 0  # an independent ran strongly: the closest analogue
            return 1 if yo[1] == same else 2  # MEDSL labels 2024 NE Senate's Osborn DEMOCRAT

        order.sort(key=rank)  # stable: keeps the midterm-first order within each rank
    for year, name in order:
        c = contests.get(ContestKey(year, state, name))
        if c is None:
            continue
        c = pool_independent(c) if independent else c
        if _usable(c):
            return c
    return None


def _turnout_totals(contests: dict[ContestKey, ContestHistory]) -> dict[str, float]:
    """state -> 2022 statewide total / 2024 statewide total (largest SEN/GOV/PRES contest each year)."""
    best: dict[tuple[int, str], int] = {}
    for k, c in contests.items():
        if k.district == "STATEWIDE" and k.office in (SEN, GOV, PRES):
            best[(k.year, k.state)] = max(best.get((k.year, k.state), 0), c.tally.total)
    return {
        st: best[(MIDTERM_YEAR, st)] / best[(2024, st)]
        for (y, st) in best
        if y == 2024 and best.get((MIDTERM_YEAR, st)) and best[(2024, st)]
    }


def _fair_by_race(mm: ops_pb2.MarketMap, fair: targets_pb2.TargetFile) -> dict[str, dict[str, float]]:
    p_by_exchange = {f.exchange_id: f.p_yes for f in fair.fair}
    out: dict[str, dict[str, float]] = {}
    for m in mm.markets:
        if m.kind != "race_party" or not m.race_id:
            continue
        parties = out.setdefault(m.race_id, {})
        if m.exchange_id in p_by_exchange:
            parties[m.party] = p_by_exchange[m.exchange_id]
    return out


def build_spec(
    market_map: Path,
    fair_live: Path,
    history_paths: Sequence[Path],
    poll_closing_path: Path = POLL_CLOSING_PATH,
    *,
    params: SimParams | None = None,
    history_labels: Iterable[str] | None = None,
) -> GeneratedSpec:
    """Build the 2026 spec rows (see the module docstring for every rule).

    `history_labels` are the strings written to the spec's `history` key (default: the paths).
    """
    params = params or SimParams()
    mm = _load_map(market_map)
    fair = from_json(targets_pb2.TargetFile, fair_live.read_text())
    contests = load_all(list(history_paths))
    closings = load_closings(poll_closing_path)
    ratios = _turnout_totals(contests)
    median_ratio = statistics.median(ratios.values()) if ratios else None
    nat_sd = math.sqrt(params.national_sd**2 + params.miss_p * params.miss_shift**2)
    out = GeneratedSpec(
        history=tuple(history_labels) if history_labels is not None else tuple(str(p) for p in history_paths),
        national_shift_prior_sd=nat_sd,
    )
    for k in contests:
        if k.district == "STATEWIDE":
            out.history_states.setdefault(k.year, set()).add(k.state)

    for rid, parties in sorted(_fair_by_race(mm, fair).items()):
        office = _office(rid)
        if office == "HOUSE":
            out.uncovered.append(Uncovered(rid, HOUSE_REASON))
            continue
        if office not in STATEWIDE_OFFICES:
            continue  # chamber control and anything else the needle does not model
        st = _state(rid)
        lp = _left_and_p(parties)
        if lp is None:
            out.uncovered.append(Uncovered(rid, "no fair value for any party row"))
            continue
        left, p_left = lp
        independent = left == IND
        base = _baseline(contests, office, st, independent)
        if base is None:
            out.uncovered.append(Uncovered(rid, f"no usable 2022/2024 statewide history for {st}"))
            continue
        if st not in closings:
            out.uncovered.append(Uncovered(rid, f"{st} missing from poll_closing.toml"))
            continue

        spec = _spec_for(race_from_id(rid), left)
        sd = analytic_margin_sd(spec, params)
        margin = margin_from_p(p_left, sd)
        notes = [f"fair p_left={p_left:.4f} ({left}) -> {margin:+.2f} pts at sd {sd:.2f}"]
        hist_margin = base.tally.margin_pts()
        z = margin / sd
        if abs(z) >= SATURATED_Z and hist_margin * margin > 0 and abs(hist_margin) > abs(margin):
            notes.append(f"saturated price: using the {base.year} {base.office} margin {hist_margin:+.2f}")
            margin = hist_margin
        if base.year == MIDTERM_YEAR:
            ratio, why = 1.0, f"baseline is the {MIDTERM_YEAR} midterm"
        elif st in ratios:
            ratio, why = ratios[st], f"{MIDTERM_YEAR}/2024 statewide totals for {st}"
        elif median_ratio is not None:
            ratio, why = median_ratio, f"no {MIDTERM_YEAR} total for {st}: median of {len(ratios)} states"
        else:
            ratio, why = 1.0, f"no state has both {MIDTERM_YEAR} and 2024 totals: default 1.0"
        notes.append(f"turnout_ratio: {why}")
        out.races.append(
            RaceRow(
                race_id=rid,
                state=st,
                year=base.year,
                office=base.office,
                left_party=left,
                p_left=p_left,
                prior_margin_mean=margin,
                prior_margin_sd=sd,
                turnout_ratio=ratio,
                poll_close_utc=closings[st].last_close_utc.strftime("%Y-%m-%dT%H:%M:%SZ"),
                national_beta=spec.national_beta,
                independent_major=independent,
                notes=tuple(notes),
            )
        )
    return out


def render_toml(spec: GeneratedSpec, *, generated_on: date, command: str, inputs: str = "") -> str:
    """The spec as TOML in the `load_spec` format, with a provenance header and uncovered list."""
    lines = [
        f"# Generated on {generated_on.isoformat()} by:",
        f"#   {command}",
        "# Do not edit by hand: re-run the command (the pre-flight does, on the VM, from fair_live.json).",
        "# Margins: prior_margin_mean = prior_margin_sd * Phi^-1(p_left), prior_margin_sd = the sim's race",
        "# margin SD (see susq/replay/spec2026.py). House races are not modelled (see the list at the end).",
    ]
    if inputs:
        lines += [f"# {line}" for line in inputs.splitlines()]
    for year in sorted(spec.history_states):
        lines.append(f"# history {year}: {len(spec.history_states[year])} states")
    hist = ", ".join(f'"{h}"' for h in spec.history)
    lines += [
        f"history = [{hist}]",
        "seed = 7",
        "horizon_h = 17.0",
        "bucket_s = 60.0",
        f"national_shift_prior_sd = {spec.national_shift_prior_sd:.3f}",
    ]
    for r in spec.races:
        lines += ["", "[[races]]", *(f"# {n}" for n in r.notes)]
        lines += [
            f'race_id = "{r.race_id}"',
            f'state = "{r.state}"',
            f"year = {r.year}",
            f'office = "{r.office}"',
            f"prior_margin_mean = {r.prior_margin_mean:.3f}",
            f"prior_margin_sd = {r.prior_margin_sd:.3f}",
            f"national_beta = {r.national_beta:.3f}",
            f"turnout_ratio = {r.turnout_ratio:.4f}",
            f'poll_close_utc = "{r.poll_close_utc}"',
        ]
        if r.independent_major:
            lines.append("independent_major = true")
    if spec.uncovered:
        lines += ["", f"# Uncovered ({len(spec.uncovered)} races):"]
        lines += [f"#   {u.race_id}: {u.reason}" for u in spec.uncovered]
    return "\n".join(lines) + "\n"


__all__ = [
    "GeneratedSpec",
    "RaceRow",
    "Uncovered",
    "build_spec",
    "cup_states",
    "margin_from_p",
    "render_toml",
]
