"""susq_models.needle: replaying county results through the service's model."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from susq_models import needle

OH = "2026-SEN-OH-S"


def result(dem: int, rep: int, batch: str, vote_type: str = "VOTE_TYPE_MAIL") -> dict[str, object]:
    return {
        "raceId": OH,
        "countyFips": "39049",
        "source": "replay",
        "voteType": vote_type,
        "votes": [{"party": "DEM", "votes": dem}, {"party": "REP", "votes": rep}],
        "totalVotes": dem + rep,
        "batchId": batch,
    }


@pytest.fixture
def model(fixtures_dir: Path) -> needle.NeedleModel:
    return needle.NeedleModel((fixtures_dir / "needle_prior.json").read_text())


def test_prior_only_posterior(model: needle.NeedleModel) -> None:
    (race,) = model.races()
    assert race["race_id"] == OH
    assert race["margin_mean"] == pytest.approx(1.5)
    assert race["p_dem"] + race["p_rep"] == pytest.approx(1.0)
    assert race["reported_frac"] == 0.0
    assert model.national() == {"mean": 0.0, "sd": pytest.approx(2.75), "n_races_informing": 0}


def test_golden_batch_bytes(model: needle.NeedleModel, fixtures_dir: Path) -> None:
    accepted = model.ingest_batch_bytes((fixtures_dir / "county_results_batch.binpb").read_bytes())
    assert accepted == 1
    assert model.counters()["unknown_race"] == 1
    (race,) = model.races()
    assert race["n_batches"] == 1
    assert 0.5 < race["reported_frac"] < 1.0
    assert model.national()["n_races_informing"] == 1


def test_dict_ingest_dedupes_and_rejects_regressions(model: needle.NeedleModel) -> None:
    assert model.ingest(result(60_000, 40_000, "b1")) == "accepted"
    assert model.ingest(result(60_000, 40_000, "b1")) == "duplicate_batch"
    assert model.ingest(result(59_000, 40_000, "b2")) == "regression"
    assert model.ingest(result(59_000, 40_000, "b3"), is_correction=True) == "accepted"
    assert model.ingest({"raceId": "nope", "countyFips": "1", "votes": []}) == "unknown_race"
    model.recompute()
    (county,) = model.counties()
    assert county["reported_votes"] == pytest.approx(99_000)


def test_mail_surprise_moves_the_race_and_the_national_shift(model: needle.NeedleModel) -> None:
    before = model.races()[0]["margin_mean"]
    # County prior 28 + mail offset 9 = D+37 expected; D+60 is a big pro-D surprise.
    model.ingest(result(80_000, 20_000, "m1"))
    model.recompute()
    assert model.races()[0]["margin_mean"] > before
    assert model.national()["mean"] > 0.0


def test_batch_json_and_proto_bytes_roundtrip(model: needle.NeedleModel, fixtures_dir: Path) -> None:
    batch = json.loads((fixtures_dir / "county_results_batch.json").read_text())
    assert model.ingest_batch(batch) == 1
    raw = model.race_posteriors_bytes()
    assert isinstance(raw, bytes)
    assert len(raw) > 0
    bus_pb2 = pytest.importorskip("susq.v1.bus_pb2")
    msg = bus_pb2.RacePosteriors.FromString(raw)
    assert msg.races[0].race_id == OH
    assert msg.races[0].p_dem == pytest.approx(model.races()[0]["p_dem"])


def test_invalid_prior_raises() -> None:
    with pytest.raises(ValueError, match="header"):
        needle.NeedleModel('{"races": []}')
