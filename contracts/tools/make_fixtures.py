#!/usr/bin/env python3
"""Regenerate the golden fixtures every language must agree on.

Python is the reference writer. For each case we emit:
  <case>.json   canonical proto3 JSON
  <case>.binpb  deterministic binary encoding
and `manifest.json` mapping case -> fully-qualified message type.
Rust and C++ tests decode both and must re-encode to exactly <case>.binpb.

Run:  uv run python contracts/tools/make_fixtures.py
"""

from __future__ import annotations

import json
from pathlib import Path

from susq.contracts import (
    bus_pb2,
    common_pb2,
    encode,
    header,
    needle_pb2,
    ops_pb2,
    targets_pb2,
    to_json,
    with_content_hash,
)

OUT = Path(__file__).resolve().parents[1] / "fixtures"
T0 = 1_791_475_200_000_000_000  # 2026-10-08T16:00:00Z in ns — fixed so fixtures are stable
SHA = "0123456789ab"


def h(producer: str, seq: int) -> common_pb2.Header:
    return header(producer, seq, now_ns=T0, sha=SHA)


def cases() -> dict[str, object]:
    c: dict[str, object] = {}

    tf = targets_pb2.TargetFile(
        header=h("python-scheduler", 7),
        tournament_slug="midterm-elections",
        valid_until_ns=T0 + 36 * 3600 * 10**9,
        inputs=[
            targets_pb2.InputHash(name="kalshi_mids", sha256="a" * 64, as_of_ns=T0 - 60 * 10**9),
            targets_pb2.InputHash(name="polls", sha256="b" * 64, as_of_ns=T0 - 3600 * 10**9),
        ],
        fair=[
            targets_pb2.FairValue(
                exchange_id="381",
                market_id="381",
                p_yes=0.86,
                p_lo=0.8,
                p_hi=0.91,
                source="0.8*kalshi+0.2*sim",
            ),
            targets_pb2.FairValue(
                exchange_id="382", market_id="382", p_yes=0.0, p_lo=0.0, p_hi=0.0, source="edge:zero"
            ),
            targets_pb2.FairValue(
                exchange_id="383", market_id="383", p_yes=1.0, p_lo=1.0, p_hi=1.0, source="edge:one"
            ),
        ],
        positions=[
            targets_pb2.TargetPosition(
                exchange_id="210",
                market_id="210",
                target_qty=-4321,
                max_entry_price=0.345,
                locked=True,
                tranche="T1",
                cluster="state:ME",
                rationale="R holds Maine (Collins)",
            ),
            targets_pb2.TargetPosition(
                exchange_id="211",
                market_id="211",
                target_qty=987,
                max_entry_price=0.12,
                locked=False,
                tranche="T2",
                cluster="national",
                rationale="idiosyncratic leg — tie hygiene ✓",
            ),
        ],
        clusters=[targets_pb2.ClusterLimit(cluster="national", max_gross_frac=0.25)],
        tourney=targets_pb2.TourneyEstimate(
            p_first=0.031,
            p_top3=0.08,
            field_size=2150,
            threshold_t_p50=17.5,
            threshold_t_p90=41.0,
            scenario="S-R-beats-polls-d7",
        ),
        notes="fixture",
    )
    c["target_file"] = with_content_hash(tf)
    c["target_file_empty"] = with_content_hash(targets_pb2.TargetFile(header=h("python-scheduler", 1)))
    c["target_pointer"] = targets_pb2.TargetPointer(
        header=h("python-scheduler", 7),
        file="targets-20261008T160000Z-deadbeef.json",
        content_sha256=c["target_file"].content_sha256,
    )  # type: ignore[attr-defined]

    c["county_results_batch"] = bus_pb2.CountyResultsBatch(
        header=h("rust-live", 42),
        results=[
            bus_pb2.CountyResult(
                race_id="2026-SEN-OH-S",
                state="OH",
                county_fips="39049",
                source="clarity:OH",
                source_ts_ns=T0,
                observed_at_ns=T0 + 1_500_000,
                vote_type=bus_pb2.VOTE_TYPE_MAIL,
                votes=[
                    bus_pb2.CandidateVotes(candidate="Sherrod Brown", party="DEM", votes=301_234),
                    bus_pb2.CandidateVotes(candidate="Jon Husted", party="REP", votes=188_765),
                ],
                total_votes=489_999,
                precincts_reporting=0,
                precincts_total=853,
                batch_id="OH-39049-0001",
            ),
            bus_pb2.CountyResult(
                race_id="2026-HOUSE-CA-13",
                state="CA",
                county_fips="06099",
                source="ca_sos",
                vote_type=bus_pb2.VOTE_TYPE_ELECTION_DAY,
                total_votes=18_446_744_073_709_551_615,
                batch_id="max-u64",
            ),
        ],
    )
    c["county_results_batch_empty"] = bus_pb2.CountyResultsBatch(header=h("rust-live", 43))
    c["race_posteriors"] = bus_pb2.RacePosteriors(
        header=h("needle", 1000),
        races=[
            bus_pb2.RacePosterior(
                race_id="2026-SEN-OH-S",
                p_dem=0.64,
                p_rep=0.36,
                margin_mean=2.1,
                margin_sd=3.4,
                expected_remaining_votes=1.25e6,
                reported_frac=0.41,
                n_batches=17,
                n_counties_reporting=52,
            ),
            bus_pb2.RacePosterior(
                race_id="2026-SEN-NE", p_dem=0.0, p_rep=0.55, margin_mean=-1.0, margin_sd=5.0
            ),
        ],
    )
    c["national_shift"] = bus_pb2.NationalShift(
        header=h("needle", 1001), mean=-1.75, sd=1.1, n_races_informing=6
    )
    c["heartbeat"] = bus_pb2.Heartbeat(header=h("needle", 1002), status="ok", last_input_seq=42)
    c["kill"] = bus_pb2.Kill(header=h("dashboard", 1), reason="human pressed Kill", issuer="dashboard")

    c["journal_entry"] = ops_pb2.JournalEntry(
        ts_ns=T0,
        producer="bridge",
        direction="response",
        method="POST",
        path="/api/v1/orders",
        body_json='{"orderId":1001,"open":true}',
        status=200,
        latency_us=84_211,
        request_id="62e5f468-4831-4522-be6c-3682fcbbe568",
        idempotency_key="k-1",
        correlation_id="c-1",
    )
    c["order_intent"] = ops_pb2.OrderIntent(
        intent_id="racer:rel-9:1791475200",
        idempotency_key="5c2e1b9e-0f7c-5b8e-9a51-2f4a6e0c7d11",
        strategy="racer",
        exchange_id="381",
        side=common_pb2.SIDE_NO,
        action=common_pb2.ACTION_BUY,
        quantity=37,
        price_pips=1450,
        expiration_ns=T0 + 30 * 10**9,
        tournament_id="t-uuid",
        state=ops_pb2.INTENT_STATE_UNKNOWN,
        created_at_ns=T0,
        updated_at_ns=T0 + 10**9,
        last_error_code="ORDER_STATUS_UNKNOWN",
        attempts=2,
    )
    c["trader_lease"] = ops_pb2.TraderLease(
        holder="bridge", host="oci-a1", pid=4242, acquired_at_ns=T0, expires_at_ns=T0 + 30 * 10**9, epoch=3
    )
    c["status_snapshot"] = ops_pb2.StatusSnapshot(
        header=h("rust-live", 55),
        mode="paper",
        stage="forage",
        trader="bridge",
        equity=100_431.5,
        cash=97_000.25,
        pnl_day=431.5,
        pnl_total=431.5,
        rank=0,
        field_size=2150,
        killed=False,
        health=[ops_pb2.HealthItem(name="platform", state="ok", last_ok_ns=T0)],
        positions=[
            ops_pb2.PositionSummary(
                exchange_id="381",
                title="Dem NH Senate",
                quantity=-37,
                avg_cost=0.145,
                mark=0.14,
                unrealized=0.185,
            )
        ],
        recent_trades=[
            ops_pb2.TradeSummary(
                ts_ns=T0,
                strategy="racer",
                title="Dem NH Senate",
                side=common_pb2.SIDE_NO,
                action=common_pb2.ACTION_BUY,
                quantity=37,
                price=0.855,
            )
        ],
        action_needed=[],
        events=["first qualifying trade filled"],
    )
    c["market_map"] = ops_pb2.MarketMap(
        header=h("parse-rules", 1),
        markets=[
            ops_pb2.MarketRule(
                market_id="381",
                exchange_id="381",
                title="Will the Democratic Party win the New Hampshire Senate?",
                race_id="2026-SEN-NH",
                kind="race_party",
                party="DEM",
                resolution_source="AP",
                kalshi_ticker="SENATENH-26-D",
                clusters=["national", "region:northeast", "state:NH"],
                verified=True,
            ),
            ops_pb2.MarketRule(
                market_id="300",
                exchange_id="300",
                race_id="2026-SEN-NE",
                kind="race_party",
                party="DEM",
                independent_can_win=True,
                verified=False,
                notes="Osborn (I): both party markets can resolve NO",
            ),
        ],
    )
    c["needle_prior"] = needle_pb2.NeedlePrior(
        header=h("replay-builder", 1),
        races=[
            needle_pb2.RacePrior(
                race_id="2026-SEN-OH-S",
                state="OH",
                prior_margin_mean=1.5,
                prior_margin_sd=5.0,
                national_beta=1.0,
                poll_close_utc="2026-11-04T00:30:00Z",
            )
        ],
        counties=[
            needle_pb2.CountyPrior(
                race_id="2026-SEN-OH-S",
                state="OH",
                county_fips="39049",
                expected_total_votes=560_000,
                expected_total_sd_frac=0.06,
                prior_margin=28.0,
                prior_margin_sd=4.0,
                vote_types=[
                    needle_pb2.VoteTypeShare(
                        vote_type=bus_pb2.VOTE_TYPE_MAIL,
                        share_of_total=0.35,
                        margin_offset=9.0,
                        typical_report_order=0.0,
                    ),
                    needle_pb2.VoteTypeShare(
                        vote_type=bus_pb2.VOTE_TYPE_ELECTION_DAY,
                        share_of_total=0.65,
                        margin_offset=-4.8,
                        typical_report_order=0.6,
                    ),
                ],
            )
        ],
        national_shift_prior_sd=2.75,
    )
    return c


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    manifest: dict[str, str] = {}
    for name, msg in cases().items():
        assert hasattr(msg, "DESCRIPTOR")
        manifest[name] = msg.DESCRIPTOR.full_name  # type: ignore[attr-defined]
        (OUT / f"{name}.json").write_text(to_json(msg))  # type: ignore[arg-type]
        (OUT / f"{name}.binpb").write_bytes(encode(msg))  # type: ignore[arg-type]

    # Forward compatibility: an unknown future field (#99, varint 7) appended to a known message.
    base = encode(cases()["heartbeat"])  # type: ignore[arg-type]
    (OUT / "heartbeat_unknown_field.binpb").write_bytes(base + bytes([0x98, 0x06, 0x07]))
    manifest["heartbeat_unknown_field"] = "susq.v1.Heartbeat@unknown"

    (OUT / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"wrote {len(manifest)} fixtures to {OUT}")


if __name__ == "__main__":
    main()
