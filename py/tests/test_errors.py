from __future__ import annotations

import pytest
from susq.platform.errors import ApiError, NetworkError, RetryClass, classify

# The docs/API.md "Error envelope" table, row by row.
TABLE = [
    (429, "RATE_LIMITED", RetryClass.RETRY_SAME_KEY),
    (409, "REQUEST_IN_FLIGHT", RetryClass.RETRY_SAME_KEY),
    (502, "ORDER_STATUS_UNKNOWN", RetryClass.RETRY_SAME_KEY),
    (503, "TX_CONFLICT", RetryClass.RETRY_SAME_KEY),
    (503, "SERVICE_UNAVAILABLE", RetryClass.RETRY_SAME_KEY),
    (400, "VALIDATION_ERROR", RetryClass.TERMINAL),
    (403, "INSUFFICIENT_SCOPES", RetryClass.TERMINAL),
    (403, "FORBIDDEN", RetryClass.TERMINAL),
    (403, "TERMS_NOT_ACKNOWLEDGED", RetryClass.TERMINAL),
    (409, "CONFLICT", RetryClass.TERMINAL),
    (422, "RELATIONSHIP_VIOLATION", RetryClass.TERMINAL),
    (401, "INVALID_API_KEY", RetryClass.TERMINAL),
    (404, "NOT_FOUND", RetryClass.TERMINAL),
    (500, "INTERNAL_ERROR", RetryClass.RETRY_READ),
    (500, "DATABASE_ERROR", RetryClass.RETRY_READ),
    (504, "HTTP_504", RetryClass.RETRY_READ),
    (503, "HTTP_503", RetryClass.RETRY_SAME_KEY),
]


@pytest.mark.parametrize(("status", "code", "expected"), TABLE)
def test_classification_matches_the_docs(status: int, code: str, expected: RetryClass) -> None:
    assert classify(status, code) is expected
    assert ApiError(status, code, "m").retry_class is expected


def test_409_conflict_is_never_retried_but_in_flight_is() -> None:
    assert not ApiError(409, "CONFLICT", "reused key").retryable_write
    assert ApiError(409, "REQUEST_IN_FLIGHT", "busy").retryable_write


def test_unknown_outcome_flags() -> None:
    assert ApiError(502, "ORDER_STATUS_UNKNOWN", "m").outcome_unknown
    assert ApiError(500, "INTERNAL_ERROR", "m").outcome_unknown
    assert not ApiError(400, "VALIDATION_ERROR", "m").outcome_unknown
    err = NetworkError("boom")
    assert err.retry_class is RetryClass.NETWORK
    assert err.outcome_unknown


def test_repr_carries_no_message_body() -> None:
    err = ApiError(429, "RATE_LIMITED", "slow down", retry_after_s=2.0, request_id="r-1")
    assert "r-1" in repr(err)
    assert err.retry_after_s == 2.0
