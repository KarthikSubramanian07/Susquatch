"""Draft market map: deterministic joins, rule flags, evidence notes, and never auto-verifying."""

from __future__ import annotations

import pytest
from susq.contracts import from_json, ops_pb2, to_json
from susq.mapping.cup import parse_markets, resolution_source
from susq.mapping.draft import build_market_map, summarize
from susq.mapping.io import read_market_map
from susq.mapping.job import draft_map
from susq.research_env import ResearchConfig

from .conftest import FIXTURES, NOW_NS


@pytest.fixture(scope="module")
def mm(cup_markets, quotes) -> ops_pb2.MarketMap:
    return build_market_map(cup_markets, quotes, seq=1, now_ns=NOW_NS)


def _row(mm: ops_pb2.MarketMap, exchange_id: str) -> ops_pb2.MarketRule:
    return next(r for r in mm.markets if r.exchange_id == exchange_id)


def test_every_row_is_unverified_and_has_a_confidence_note(mm) -> None:
    assert len(mm.markets) == 28  # 26 single-exchange markets + one two-exchange market
    assert not any(r.verified for r in mm.markets)
    assert all(r.notes.startswith("confidence=") for r in mm.markets)
    assert mm.header.producer.name == "python-scheduler"
    assert mm.header.seq == 1


def test_exact_party_join_on_both_venues(mm) -> None:
    nh = _row(mm, "1381")
    assert (nh.race_id, nh.kind, nh.party) == ("2026-SEN-NH", "race_party", "DEM")
    assert nh.kalshi_ticker == "SENATENH-26-D"
    assert not nh.kalshi_inverted
    assert nh.polymarket_token_id
    assert not nh.polymarket_inverted
    assert nh.resolution_source == "Associated Press race call"
    assert nh.clusters[:3] == ["national", "region:northeast", "state:NH"]
    assert nh.notes.startswith("confidence=1.00")


def test_special_election_rcv_and_named_clusters(mm) -> None:
    oh = _row(mm, "1209")
    assert oh.race_id == "2026-SEN-OH-S"
    assert {"red-state-reach", "midwest-industrial"} <= set(oh.clusters)
    ak = _row(mm, "1213")
    assert ak.rcv
    assert "red-state-reach" in ak.clusters
    me = _row(mm, "1203")
    assert me.rcv
    assert "new-england-incumbency" in me.clusters


def test_chamber_markets_are_manually_settled(mm) -> None:
    house_d, senate_r = _row(mm, "1151"), _row(mm, "1154")
    assert (house_d.kind, house_d.race_id, house_d.party) == ("chamber", "2026-CONTROL-HOUSE", "DEM")
    assert house_d.manual_settlement
    assert senate_r.manual_settlement
    assert house_d.clusters == ["national", "house"]
    assert senate_r.clusters == ["national"]
    assert house_d.kalshi_ticker == "CONTROLH-2026-D"


def test_independent_race_never_uses_a_complement(mm) -> None:
    osborn, ricketts = _row(mm, "1218"), _row(mm, "1219")
    assert osborn.party == "IND"
    assert osborn.independent_can_win
    assert ricketts.party == "REP"
    assert ricketts.independent_can_win
    assert not ricketts.kalshi_inverted
    assert not ricketts.polymarket_inverted
    assert "not complementary" in ricketts.notes


def test_house_seats_and_governor(mm) -> None:
    ny = _row(mm, "1301")
    assert ny.race_id == "2026-HOUSE-NY-17"
    assert "house" in ny.clusters
    pa = _row(mm, "1303")
    assert pa.race_id == "2026-HOUSE-PA-07"
    assert pa.kalshi_ticker.startswith("HOUSEPA7-26")
    ga = _row(mm, "1220")
    assert ga.race_id == "2026-GOV-GA"
    assert ga.runoff_possible
    assert ga.kalshi_ticker == "GOVPARTYGA-26-D"


def test_ticker_title_quirk_is_resolved_by_title(mm) -> None:
    ky = _row(mm, "1217")
    assert ky.race_id == "2026-SEN-KY"
    assert ky.kalshi_ticker == "SENATELA-26-D"


def test_multi_outcome_market_uses_exchange_options(mm) -> None:
    d, r = _row(mm, "1360"), _row(mm, "1361")
    assert (d.race_id, d.party) == ("2026-GOV-IA", "DEM")
    assert (r.race_id, r.party) == ("2026-GOV-IA", "REP")
    assert d.market_id == r.market_id == "360"


def test_unpriced_and_freeform_rows_are_penalized(mm) -> None:
    nc = _row(mm, "1205")  # NC Senate is not in the venue fixture
    assert "kalshi=none" in nc.notes
    assert "sim-only" in nc.notes
    assert nc.notes.startswith("confidence=0.60")
    free = _row(mm, "1388")
    assert free.kind == "freeform"
    assert not free.race_id
    assert free.notes.startswith("confidence=0.00")


def test_cup_price_far_from_real_money_is_flagged(mm) -> None:
    mi_r = _row(mm, "1216")  # Cup 0.80 vs ~0.30 real money
    assert "PRICE_DISAGREES" in mi_r.notes
    assert summarize(mm)["flagged"] == 1


def test_complement_used_only_when_two_party(cup_markets, quotes) -> None:
    only_rep = [q for q in quotes if not (q.race_id == "2026-SEN-TX" and q.party == "DEM")]
    mm = build_market_map(cup_markets, only_rep, seq=2, now_ns=NOW_NS)
    tx_d = _row(mm, "1201")
    assert tx_d.kalshi_ticker == "SENATETX-26-R"
    assert tx_d.kalshi_inverted
    assert "inverted" in tx_d.notes


def test_existing_verified_rows_are_kept_verbatim(cup_markets, quotes) -> None:
    human = ops_pb2.MarketRule(
        market_id="381", exchange_id="1381", race_id="2026-SEN-NH", party="DEM", kind="race_party",
        kalshi_ticker="HUMAN-CHOSEN", verified=True, notes="checked by hand",
    )  # fmt: skip
    existing = ops_pb2.MarketMap(markets=[human])
    mm = build_market_map(cup_markets, quotes, seq=3, existing=existing, now_ns=NOW_NS)
    kept = _row(mm, "1381")
    assert kept == human
    assert sum(r.verified for r in mm.markets) == 1


def test_proto_json_roundtrip(mm) -> None:
    assert from_json(ops_pb2.MarketMap, to_json(mm)) == mm


def test_node_resolution_source_helper() -> None:
    assert resolution_source(None) == ""
    tree = {"root": {"nodeType": "operator", "children": [{"contractDetails": {"source": "SoS"}}]}}
    assert resolution_source(tree) == "SoS"


def test_parse_markets_accepts_all_recorded_shapes() -> None:
    row = {"id": 1, "title": "t", "exchanges": [{"id": 2, "option": None, "latestPrice": None}]}
    for doc in ({"data": [row]}, [row], [{"data": [row]}]):
        (m,) = parse_markets(doc)
        assert m.market_id == "1"
        assert m.exchanges[0].exchange_id == "2"
        assert m.exchanges[0].latest_price is None
    with pytest.raises(ValueError, match="unrecognized"):
        parse_markets("nope")


def test_draft_job_writes_live_only_once(home, quotes) -> None:
    cup = home.data / "cup" / "markets"
    cup.mkdir(parents=True)
    (cup / "markets-20260930T160000Z.json").write_text((FIXTURES / "cup_markets.json").read_text())
    cfg = ResearchConfig()
    _, live = draft_map(home, cfg, quotes, now_ns=NOW_NS)
    assert live
    assert home.market_map.exists()
    assert home.market_map_draft.exists()
    # A human verifies a row; the next draft must not clobber the live map...
    live_map = read_market_map(home.market_map)
    assert live_map is not None
    live_map.markets[0].verified = True
    home.market_map.write_text(to_json(live_map))
    _, live2 = draft_map(home, cfg, quotes, now_ns=NOW_NS)
    assert not live2
    assert read_market_map(home.market_map).markets[0].verified
    # ...and even an explicit promotion keeps the verified row.
    _, live3 = draft_map(home, cfg, quotes, now_ns=NOW_NS, promote=True)
    assert live3
    assert sum(r.verified for r in read_market_map(home.market_map).markets) == 1
    assert read_market_map(home.market_map_draft).header.seq == 3


def test_draft_job_needs_a_cup_listing(home, quotes) -> None:
    with pytest.raises(FileNotFoundError):
        draft_map(home, ResearchConfig(), quotes, now_ns=NOW_NS)
