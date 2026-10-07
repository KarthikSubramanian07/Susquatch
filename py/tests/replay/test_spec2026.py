"""The 2026 spec builder on a tiny synthetic market map, fair file and county history."""

from __future__ import annotations

import json
import statistics
from datetime import date
from pathlib import Path

import pytest
from susq.replay.__main__ import app
from susq.replay.prior import weighted_prior_margin
from susq.replay.spec import load_spec, materialize_prior
from susq.replay.spec2026 import (
    HOUSE_REASON,
    GeneratedSpec,
    build_spec,
    cup_states,
    margin_from_p,
    render_toml,
)
from typer.testing import CliRunner

HEADER = (
    "year,state,state_po,county_name,county_fips,office,district,candidate,party,mode,"
    "candidatevotes,totalvotes"
)

# (race id, party, exchange id, fair p_yes or None for "no fair row")
MARKETS: list[tuple[str, str, str, float | None]] = [
    ("2026-SEN-GA", "DEM", "1", 0.60),
    ("2026-SEN-GA", "REP", "2", 0.42),  # 2% overround: p_left = 0.60 / 1.02
    ("2026-GOV-GA", "DEM", "3", 0.30),
    ("2026-GOV-GA", "REP", "4", 0.70),
    ("2026-SEN-NE", "DEM", "5", 0.03),
    ("2026-SEN-NE", "IND", "6", 0.35),  # the independent out-prices the Democrat: left side is IND
    ("2026-SEN-NE", "REP", "7", 0.65),
    ("2026-SEN-DE", "DEM", "8", 0.99),  # saturated: the 2024 margin (+20) replaces sd * Phi^-1
    ("2026-SEN-DE", "REP", "9", 0.01),
    ("2026-GOV-WY", "DEM", "10", 0.02),  # no history for WY
    ("2026-GOV-WY", "REP", "11", 0.98),
    ("2026-SEN-ME", "DEM", "12", None),  # listed but unpriced
    ("2026-HOUSE-CA-22", "DEM", "13", 0.5),
    ("2026-HOUSE-CA-22", "REP", "14", 0.5),
]


def _county_rows(
    year: int, st: str, name: str, office: str, fips: str, d: int, r: int, o: int = 0
) -> list[str]:
    total = d + r + o
    rows = [
        f"{year},{name},{st},C{fips},{fips},{office},STATEWIDE,DEMCAND,DEMOCRAT,TOTAL,{d},{total}",
        f"{year},{name},{st},C{fips},{fips},{office},STATEWIDE,REPCAND,REPUBLICAN,TOTAL,{r},{total}",
    ]
    if o:
        rows.append(f"{year},{name},{st},C{fips},{fips},{office},STATEWIDE,INDCAND,OTHER,TOTAL,{o},{total}")
    return rows


@pytest.fixture
def inputs(tmp_path: Path) -> tuple[Path, Path, list[Path]]:
    mm = {
        "markets": [
            {"marketId": e, "exchangeId": e, "raceId": rid, "kind": "race_party", "party": party}
            for rid, party, e, _ in MARKETS
        ]
        + [
            {
                "marketId": "99",
                "exchangeId": "99",
                "raceId": "2026-CONTROL-SENATE",
                "kind": "chamber",
                "party": "DEM",
            }
        ]
    }
    fair = {"fair": [{"exchangeId": e, "pYes": p} for _, _, e, p in MARKETS if p is not None]}
    (tmp_path / "market_map.json").write_text(json.dumps(mm))
    (tmp_path / "fair_live.json").write_text(json.dumps(fair))
    h22 = [HEADER]
    h24 = [HEADER]
    for fips, d, r in (("13001", 600, 400), ("13003", 3000, 5000), ("13005", 100, 900)):
        h22 += _county_rows(2022, "GA", "GEORGIA", "US SENATE", fips, d, r, 20)
        h22 += _county_rows(2022, "GA", "GEORGIA", "GOVERNOR", fips, d - 50, r + 50)
        h24 += _county_rows(2024, "GA", "GEORGIA", "US PRESIDENT", fips, 2 * d, 2 * r)
    for fips, ind, r in (("31001", 700, 900), ("31003", 2000, 1500)):
        h24 += _county_rows(2024, "NE", "NEBRASKA", "US SENATE", fips, 0, r, ind)
    for fips, d, r in (("10001", 3000, 2000), ("10003", 6000, 4000)):
        h24 += _county_rows(2024, "DE", "DELAWARE", "US SENATE", fips, d, r)
    p22, p24 = tmp_path / "h22.csv", tmp_path / "h24.csv"
    p22.write_text("\n".join(h22) + "\n")
    p24.write_text("\n".join(h24) + "\n")
    return tmp_path / "market_map.json", tmp_path / "fair_live.json", [p22, p24]


@pytest.fixture
def built(inputs: tuple[Path, Path, list[Path]]) -> GeneratedSpec:
    mm, fair, hist = inputs
    return build_spec(mm, fair, hist)


def test_cup_states_are_senate_and_governor_only(inputs: tuple[Path, Path, list[Path]]) -> None:
    assert cup_states(inputs[0]) == {"GA", "NE", "DE", "WY", "ME"}


def test_margin_inverts_the_sims_probability_map() -> None:
    nd = statistics.NormalDist()
    for p in (0.1, 0.4, 0.5, 0.77):
        m = margin_from_p(p, 5.13)
        assert nd.cdf(m / 5.13) == pytest.approx(p, abs=1e-9)
    assert margin_from_p(1.0, 5.0) == pytest.approx(margin_from_p(0.999, 5.0))


def test_races_covered_and_uncovered(built: GeneratedSpec) -> None:
    rows = {r.race_id: r for r in built.races}
    assert set(rows) == {"2026-SEN-GA", "2026-GOV-GA", "2026-SEN-NE", "2026-SEN-DE"}
    why = {u.race_id: u.reason for u in built.uncovered}
    assert why["2026-GOV-WY"] == "no usable 2022/2024 statewide history for WY"
    assert why["2026-SEN-ME"] == "no fair value for any party row"
    assert why["2026-HOUSE-CA-22"] == HOUSE_REASON
    assert "2026-CONTROL-SENATE" not in why
    assert built.history_states == {2022: {"GA"}, 2024: {"GA", "NE", "DE"}}


def test_margin_sd_and_baselines(built: GeneratedSpec) -> None:
    rows = {r.race_id: r for r in built.races}
    ga = rows["2026-SEN-GA"]
    assert (ga.year, ga.office, ga.left_party) == (2022, "US SENATE", "DEM")
    assert ga.p_left == pytest.approx(0.60 / 1.02)
    assert 5.0 < ga.prior_margin_sd < 5.5, "the sim's unconditional race SD"
    assert ga.prior_margin_mean == pytest.approx(margin_from_p(ga.p_left, ga.prior_margin_sd))
    assert ga.turnout_ratio == 1.0
    assert "2022 midterm" in ga.notes[-1]
    assert ga.poll_close_utc == "2026-11-04T00:00:00Z"
    assert rows["2026-GOV-GA"].office == "GOVERNOR"

    ne = rows["2026-SEN-NE"]
    assert ne.left_party == "IND"
    assert ne.independent_major
    assert (ne.year, ne.office) == (2024, "US SENATE")
    assert ne.prior_margin_sd > ga.prior_margin_sd, "independents get the larger race SD"
    assert ne.p_left == pytest.approx(0.35 / 1.00)
    ga_ratio = (1000 + 8000 + 1000 + 60) / (2 * (1000 + 8000 + 1000))
    assert ne.turnout_ratio == pytest.approx(ga_ratio), "no NE 2022 total: median over states with both"
    assert "median of 1 states" in ne.notes[-1]

    de = rows["2026-SEN-DE"]
    assert de.prior_margin_mean == pytest.approx(20.0), "saturated price uses the larger historical margin"
    assert any("saturated" in n for n in de.notes)


def test_rendered_spec_loads_and_builds_a_consistent_prior(built: GeneratedSpec, tmp_path: Path) -> None:
    text = render_toml(built, generated_on=date(2026, 10, 6), command="python -m susq.replay spec2026 ...")
    assert text.startswith("# Generated on 2026-10-06 by:")
    assert "2026-HOUSE-CA-22: House" in text
    path = tmp_path / "general_2026.toml"
    path.write_text(text)
    spec = load_spec(path)
    assert [r.race_id for r in spec.races] == [r.race_id for r in built.races]
    prior = materialize_prior(spec)
    assert prior.national_shift_prior_sd == pytest.approx(built.national_shift_prior_sd, abs=1e-3)
    by_id = {r.race_id: r for r in prior.races}
    for row in built.races:
        rp = by_id[row.race_id]
        assert rp.prior_margin_mean == pytest.approx(row.prior_margin_mean, abs=1e-3)
        assert rp.prior_margin_sd == pytest.approx(row.prior_margin_sd, abs=1e-3)
        assert rp.poll_close_utc == row.poll_close_utc
        assert rp.independent_major == row.independent_major
        assert weighted_prior_margin(prior, row.race_id) == pytest.approx(row.prior_margin_mean, abs=0.5)
    ne_counties = [c for c in prior.counties if c.race_id == "2026-SEN-NE"]
    assert len(ne_counties) == 2
    hist = {"31001": 1600, "31003": 3500}
    for c in ne_counties:
        assert c.expected_total_votes == pytest.approx(
            hist[c.county_fips] * next(r for r in built.races if r.race_id == "2026-SEN-NE").turnout_ratio
        )


def test_cli_writes_the_spec(inputs: tuple[Path, Path, list[Path]], tmp_path: Path) -> None:
    mm, fair, (h22, h24) = inputs
    out = tmp_path / "out" / "general_2026.toml"
    args = ["spec2026", "--market-map", str(mm), "--fair", str(fair), "--history", str(h22)]
    res = CliRunner().invoke(app, [*args, "--history", str(h24), "--out", str(out)])
    assert res.exit_code == 0, res.output
    assert "4 races, 3 uncovered" in res.output
    assert f"--history {h22} --history {h24}" in out.read_text()
    assert len(load_spec(out).races) == 4

    missing = CliRunner().invoke(app, [*args[:5], "--history", str(tmp_path / "nope.csv"), "--out", str(out)])
    assert missing.exit_code != 0
