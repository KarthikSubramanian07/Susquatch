from __future__ import annotations

from datetime import UTC, datetime, timedelta

import pytest
from susq.contracts import idempotency_key
from susq.platform.models import (
    BatchResult,
    ConstraintEvaluation,
    Leaderboard,
    OrderRequest,
    PlaceOrderResult,
    RealtimeToken,
    as_qty,
    iso_utc,
)
from susq.platform.prices import PriceError

NS = "6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99"
EXP = datetime.fromtimestamp(1791475230, UTC)


def order(**kw: object) -> OrderRequest:
    base: dict[str, object] = {
        "exchange_id": "381",
        "side": "no",
        "action": "buy",
        "quantity": 37,
        "price_pips": 1450,
        "expiration": EXP,
        "tournament_id": "t-uuid",
    }
    base.update(kw)
    return OrderRequest(**base)  # type: ignore[arg-type]


def test_runtime_md_idempotency_vector() -> None:
    req = order()
    assert req.canonical_payload() == "381|no|buy|37|1450|1791475230|t-uuid"
    assert (
        idempotency_key(NS, "racer:rel-9:1791475200", req.canonical_payload())
        == "c5fe4b7e-7efc-55fb-82b5-1bd6356d699c"
    )


def test_order_body_is_wire_shaped() -> None:
    assert order().to_body() == {
        "exchangeId": "381",
        "side": "no",
        "action": "buy",
        "quantity": 37,
        "price": 0.145,
        "expirationDate": "2026-10-08T16:00:30Z",
        "tournamentId": "t-uuid",
    }


@pytest.mark.parametrize(
    ("kw", "err"),
    [
        ({"price_pips": 1451}, PriceError),
        ({"price_pips": 0}, PriceError),
        ({"price_pips": 10_000}, PriceError),
        ({"quantity": 0}, ValueError),
        ({"side": "maybe"}, ValueError),
        ({"action": "hold"}, ValueError),
        ({"expiration": datetime(2026, 10, 1)}, ValueError),
        ({"tournament_id": ""}, ValueError),
    ],
)
def test_order_request_refuses_what_the_platform_would(kw: dict[str, object], err: type[Exception]) -> None:
    with pytest.raises(err):
        order(**kw)


def test_yes_space_helpers() -> None:
    assert order(side="no", action="buy").yes_direction == -1
    assert order(side="yes", action="sell").yes_direction == -1
    assert order(side="no", action="sell").yes_direction == 1
    assert order(side="no", price_pips=1450).yes_price_pips == 8550


def test_models_tolerate_unknown_and_missing_fields() -> None:
    res = PlaceOrderResult.model_validate(
        {"orderId": 7, "exchangeId": "1", "open": True, "brandNewField": {"x": 1}, "quantityTraded": 0}
    )
    assert res.order_id == 7
    assert res.model_extra == {"brandNewField": {"x": 1}}
    lb = Leaderboard.model_validate({"leaderboard": [], "total": 0, "myRank": None, "season": None})
    assert lb.my_rank is None


def test_batch_item_error_code_both_shapes() -> None:
    batch = BatchResult.model_validate(
        {
            "results": [
                {"index": 0, "ok": False, "status": 429, "data": {"code": "RATE_LIMITED"}},
                {"index": 1, "ok": False, "status": 400, "data": {"error": {"code": "VALIDATION_ERROR"}}},
                {"index": 2, "ok": True, "status": 200, "data": {"orderId": 1, "exchangeId": "9"}},
            ]
        }
    )
    assert [r.error_code for r in batch.results] == ["RATE_LIMITED", "VALIDATION_ERROR", None]
    assert batch.results[2].order() is not None


def test_constraint_evaluation_parses_the_documented_shape() -> None:
    ev = ConstraintEvaluation.model_validate(
        {
            "relationshipId": "r",
            "type": "mutually_exclusive",
            "violationAmount": 0.05,
            "reason": "x",
            "suggestedCorrectiveTrades": [
                {
                    "exchangeId": "36",
                    "outcomeSide": "No",
                    "action": "Buy",
                    "rationale": "r",
                    "marketId": None,
                    "marketTitle": None,
                    "outcome": None,
                    "currentPrice": 0.6,
                }
            ],
            "observedPrices": None,
            "bound": 1,
            "lowerBound": None,
            "upperBound": 1,
            "currentValue": 1.05,
            "evaluationStatus": "violated",
            "diagnostic": None,
            "tournamentId": None,
            "direction": {"kind": "symmetric", "fromExchangeIds": [], "toExchangeIds": [], "description": ""},
            "constraint": {
                "description": "",
                "priceRule": "",
                "boundSemantics": "",
                "evaluationEndpoint": "",
            },
        }
    )
    assert ev.suggested_corrective_trades[0].outcome_side == "No"


def test_realtime_token_repr_hides_the_jwt() -> None:
    tok = RealtimeToken.model_validate(
        {
            "token": "secret-jwt-value",
            "expiresAt": "2026-10-01T19:00:00Z",
            "supabaseUrl": "https://x.supabase.co",
            "anonKey": "anon",
            "channels": {"user": "user:p"},
        }
    )
    assert "secret-jwt-value" not in repr(tok)


def test_helpers() -> None:
    assert as_qty(None) == 0
    assert as_qty(3.0) == 3
    assert (
        iso_utc(datetime(2026, 11, 4, 16, 55, tzinfo=UTC) + timedelta(microseconds=5))
        == "2026-11-04T16:55:00Z"
    )
    with pytest.raises(ValueError, match="naive"):
        iso_utc(datetime(2026, 1, 1))
