//! Typed platform errors and the retry classes from `docs/API.md`.
//!
//! Callers branch on [`RetryClass`], never on messages. Only the codes the docs call safe are retried,
//! and always with the same idempotency key.

use std::time::Duration;

use serde_json::Value;

/// Minimum wait before retrying a `409 REQUEST_IN_FLIGHT` (per the error table).
pub const REQUEST_IN_FLIGHT_WAIT: Duration = Duration::from_secs(90);

/// An error envelope `{"error":{"code","message","details"}}` plus transport metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    /// Stable machine code (`RATE_LIMITED`, `VALIDATION_ERROR`, ...). `HTTP_<status>` if the body
    /// carried no envelope.
    pub code: String,
    pub message: String,
    pub details: Value,
    /// Parsed `Retry-After` header (seconds form).
    pub retry_after: Option<Duration>,
    /// `x-request-id` header, for support and the journal.
    pub request_id: Option<String>,
}

/// What the caller may do after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Documented as safe: retry the identical request with the same key, not before `min_wait`.
    Retry { min_wait: Option<Duration> },
    /// Unknown server failure (500 `INTERNAL_ERROR`/`DATABASE_ERROR`, undecodable 2xx). Reads may
    /// retry; writes must reconcile first because the outcome is unknown.
    Unknown,
    /// Every other 4xx: do not retry.
    Terminal,
}

impl ApiError {
    /// Classify per `docs/API.md` §Error envelope.
    pub fn retry_class(&self) -> RetryClass {
        match (self.status, self.code.as_str()) {
            (429, _) => RetryClass::Retry {
                min_wait: self.retry_after,
            },
            (409, "REQUEST_IN_FLIGHT") => RetryClass::Retry {
                min_wait: Some(self.retry_after.unwrap_or_default().max(REQUEST_IN_FLIGHT_WAIT)),
            },
            (502, "ORDER_STATUS_UNKNOWN") => RetryClass::Retry {
                min_wait: self.retry_after,
            },
            (503, _) => RetryClass::Retry {
                min_wait: self.retry_after,
            },
            (500..=599, _) => RetryClass::Unknown,
            _ => RetryClass::Terminal,
        }
    }

    /// Shorthand constructor used by the mock and tests.
    pub fn new(status: u16, code: &str, message: &str) -> Self {
        Self {
            status,
            code: code.to_owned(),
            message: message.to_owned(),
            details: Value::Null,
            retry_after: None,
            request_id: None,
        }
    }

    /// Builder-style `Retry-After`.
    pub fn with_retry_after(mut self, d: Duration) -> Self {
        self.retry_after = Some(d);
        self
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)
    }
}

/// Every way a platform call can fail.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlatformError {
    /// The server answered with an error status.
    #[error("api error {0}")]
    Api(Box<ApiError>),
    /// Transport failure. For writes the request may or may not have reached the engine.
    #[error("network error: {message}")]
    Network { message: String, timeout: bool },
    /// The server answered 2xx but the body did not decode. For writes the outcome is unknown.
    #[error("undecodable {status} response: {message}")]
    Decode { status: u16, message: String },
    /// Rejected client-side before sending (bad id, >100 ids, ...). Nothing was sent.
    #[error("invalid request: {0}")]
    Invalid(String),
}

impl PlatformError {
    /// Retry class of this failure.
    pub fn retry_class(&self) -> RetryClass {
        match self {
            PlatformError::Api(e) => e.retry_class(),
            PlatformError::Network { .. } => RetryClass::Retry { min_wait: None },
            PlatformError::Decode { .. } => RetryClass::Unknown,
            PlatformError::Invalid(_) => RetryClass::Terminal,
        }
    }

    /// The API code, if the server sent one.
    pub fn code(&self) -> Option<&str> {
        match self {
            PlatformError::Api(e) => Some(&e.code),
            _ => None,
        }
    }

    /// HTTP status, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            PlatformError::Api(e) => Some(e.status),
            PlatformError::Decode { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// `Retry-After`, if the server sent one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            PlatformError::Api(e) => e.retry_after,
            _ => None,
        }
    }

    /// True for `429 RATE_LIMITED` (feeds the adaptive limiter).
    pub fn is_rate_limited(&self) -> bool {
        self.status() == Some(429)
    }

    /// Short label for metrics/journal (`RATE_LIMITED`, `NETWORK`, `DECODE`, `INVALID`).
    pub fn label(&self) -> &str {
        match self {
            PlatformError::Api(e) => &e.code,
            PlatformError::Network { .. } => "NETWORK",
            PlatformError::Decode { .. } => "DECODE",
            PlatformError::Invalid(_) => "INVALID",
        }
    }
}

impl From<ApiError> for PlatformError {
    fn from(e: ApiError) -> Self {
        PlatformError::Api(Box::new(e))
    }
}

/// Result alias for platform calls.
pub type PlatformResult<T> = Result<T, PlatformError>;

#[cfg(test)]
mod tests {
    use super::*;

    fn class(status: u16, code: &str) -> RetryClass {
        ApiError::new(status, code, "").retry_class()
    }

    #[test]
    fn documented_retryable_codes() {
        assert_eq!(class(429, "RATE_LIMITED"), RetryClass::Retry { min_wait: None });
        assert_eq!(
            class(502, "ORDER_STATUS_UNKNOWN"),
            RetryClass::Retry { min_wait: None }
        );
        assert_eq!(class(503, "TX_CONFLICT"), RetryClass::Retry { min_wait: None });
        assert_eq!(
            class(503, "SERVICE_UNAVAILABLE"),
            RetryClass::Retry { min_wait: None }
        );
        assert_eq!(
            class(409, "REQUEST_IN_FLIGHT"),
            RetryClass::Retry {
                min_wait: Some(REQUEST_IN_FLIGHT_WAIT)
            }
        );
    }

    #[test]
    fn everything_else_4xx_is_terminal() {
        for (s, c) in [
            (400, "VALIDATION_ERROR"),
            (403, "INSUFFICIENT_SCOPES"),
            (403, "TERMS_NOT_ACKNOWLEDGED"),
            (409, "CONFLICT"),
            (422, "RELATIONSHIP_VIOLATION"),
            (404, "NOT_FOUND"),
            (401, "INVALID_API_KEY"),
        ] {
            assert_eq!(class(s, c), RetryClass::Terminal, "{s} {c}");
        }
    }

    #[test]
    fn unknown_server_errors() {
        assert_eq!(class(500, "INTERNAL_ERROR"), RetryClass::Unknown);
        assert_eq!(class(500, "DATABASE_ERROR"), RetryClass::Unknown);
        assert_eq!(class(502, "BAD_GATEWAY"), RetryClass::Unknown);
    }

    #[test]
    fn retry_after_is_honoured_and_in_flight_waits_at_least_90s() {
        let e = ApiError::new(429, "RATE_LIMITED", "").with_retry_after(Duration::from_secs(7));
        assert_eq!(
            e.retry_class(),
            RetryClass::Retry {
                min_wait: Some(Duration::from_secs(7))
            }
        );
        let e = ApiError::new(409, "REQUEST_IN_FLIGHT", "").with_retry_after(Duration::from_secs(5));
        assert_eq!(
            e.retry_class(),
            RetryClass::Retry {
                min_wait: Some(REQUEST_IN_FLIGHT_WAIT)
            }
        );
        let e = ApiError::new(409, "REQUEST_IN_FLIGHT", "").with_retry_after(Duration::from_secs(120));
        assert_eq!(
            e.retry_class(),
            RetryClass::Retry {
                min_wait: Some(Duration::from_secs(120))
            }
        );
    }

    #[test]
    fn transport_classes() {
        let n = PlatformError::Network {
            message: "reset".into(),
            timeout: false,
        };
        assert_eq!(n.retry_class(), RetryClass::Retry { min_wait: None });
        assert_eq!(
            PlatformError::Invalid("x".into()).retry_class(),
            RetryClass::Terminal
        );
        assert_eq!(
            PlatformError::Decode {
                status: 200,
                message: "eof".into()
            }
            .retry_class(),
            RetryClass::Unknown
        );
    }
}
