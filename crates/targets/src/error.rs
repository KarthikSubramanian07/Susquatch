//! Typed reasons a target file (or the pointer to it) is refused.

use std::{fmt, path::PathBuf};

/// Which of the two documents a header problem was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Document {
    /// `targets/current.json` in pointer form.
    Pointer,
    /// The `TargetFile` itself (named by the pointer, or `current.json` in direct form).
    TargetFile,
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Document::Pointer => "pointer",
            Document::TargetFile => "target file",
        })
    }
}

/// Why the latest `current.json` was not adopted, or why the adopted file is no longer fresh.
///
/// Every variant is fail-safe: the reader keeps the last good targets but stops allowing new exposure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RejectReason {
    /// `current.json` does not exist.
    #[error("pointer {} missing", path.display())]
    PointerMissing { path: PathBuf },
    /// `current.json` exists but could not be read.
    #[error("pointer {} unreadable: {message}", path.display())]
    PointerUnreadable { path: PathBuf, message: String },
    /// `current.json` is not a JSON object or not a valid `TargetPointer`.
    #[error("pointer unparseable: {message}")]
    PointerUnparseable { message: String },
    /// The pointer names something other than a plain file inside the targets directory.
    #[error("pointer names unsafe file {name:?}")]
    UnsafeFileName { name: String },
    /// The file named by the pointer does not exist.
    #[error("target file {file} missing")]
    FileMissing { file: String },
    /// The file named by the pointer exists but could not be read.
    #[error("target file {file} unreadable: {message}")]
    FileUnreadable { file: String, message: String },
    /// The target file is not valid UTF-8 / proto3 JSON for `TargetFile`.
    #[error("target file {file} unparseable: {message}")]
    FileUnparseable { file: String, message: String },
    /// A document has no header.
    #[error("{doc} header missing")]
    MissingHeader { doc: Document },
    /// A document was produced for a different schema version.
    #[error("{doc} schema version {got} not supported (want {want})")]
    SchemaVersion { doc: Document, got: u32, want: u32 },
    /// The pointer's `content_sha256` differs from the one declared in the file.
    #[error("pointer hash {pointer} does not match file hash {file}")]
    PointerHashMismatch { pointer: String, file: String },
    /// The file's declared `content_sha256` differs from the recomputed hash.
    #[error("content hash mismatch: declared {declared}, computed {computed}")]
    ContentHashMismatch { declared: String, computed: String },
    /// The file targets a different tournament than this process trades.
    #[error("tournament {got:?} does not match configured {want:?}")]
    WrongTournament { got: String, want: String },
    /// `valid_until_ns` has passed.
    #[error("expired {} ago", span(.now_ns.saturating_sub(*.valid_until_ns)))]
    Expired { valid_until_ns: i64, now_ns: i64 },
    /// `header.produced_at_ns` is older than the configured maximum age.
    #[error(
        "too old: produced {} ago, max age {}",
        span(.now_ns.saturating_sub(*.produced_at_ns)),
        span(*.max_age_ns)
    )]
    TooOld {
        produced_at_ns: i64,
        now_ns: i64,
        max_age_ns: i64,
    },
    /// `header.produced_at_ns` is further in the future than the allowed clock skew.
    #[error("produced {} in the future", span(.produced_at_ns.saturating_sub(*.now_ns)))]
    ProducedInFuture { produced_at_ns: i64, now_ns: i64 },
    /// A value inside the file failed validation.
    #[error("invalid content: {0}")]
    InvalidContent(#[from] ContentError),
}

/// A value inside an otherwise well-formed target file that must not be traded on.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ContentError {
    /// A fair value has an empty `exchange_id`.
    #[error("fair value #{index} has an empty exchange id")]
    EmptyFairId { index: usize },
    /// Two fair values share an `exchange_id`.
    #[error("duplicate fair value for exchange {exchange_id}")]
    DuplicateFair { exchange_id: String },
    /// `p_yes`, `p_lo` or `p_hi` is not a finite number in [0, 1].
    #[error("fair value {exchange_id}: {field} = {value} is not a probability")]
    Probability {
        exchange_id: String,
        field: &'static str,
        value: f64,
    },
    /// The band is not ordered `p_lo <= p_yes <= p_hi`.
    #[error("fair value {exchange_id}: band not ordered (p_lo {p_lo}, p_yes {p_yes}, p_hi {p_hi})")]
    BandOrder {
        exchange_id: String,
        p_lo: f64,
        p_yes: f64,
        p_hi: f64,
    },
    /// A target position has an empty `exchange_id`.
    #[error("position #{index} has an empty exchange id")]
    EmptyPositionId { index: usize },
    /// Two target positions share an `exchange_id`.
    #[error("duplicate position for exchange {exchange_id}")]
    DuplicatePosition { exchange_id: String },
    /// `max_entry_price` is not a finite number strictly inside (0, 1).
    #[error("position {exchange_id}: max_entry_price {value} not in (0, 1)")]
    MaxEntryPrice { exchange_id: String, value: f64 },
    /// `target_qty` does not fit the platform's 32-bit quantity.
    #[error("position {exchange_id}: target_qty {qty} outside the i32 range")]
    TargetQty { exchange_id: String, qty: i64 },
    /// `max_gross_frac` is not a finite number in [0, 1].
    #[error("cluster {cluster:?}: max_gross_frac {value} not in [0, 1]")]
    ClusterFrac { cluster: String, value: f64 },
    /// A tournament probability is not a finite number in [0, 1].
    #[error("tourney {field} = {value} is not a probability")]
    TourneyProbability { field: &'static str, value: f64 },
    /// A tournament threshold is not finite.
    #[error("tourney {field} = {value} is not finite")]
    TourneyNotFinite { field: &'static str, value: f64 },
}

/// Human-readable length of a nanosecond span ("45s", "12m", "3.2h", "4.5d"); sign is ignored.
pub(crate) fn span(ns: i64) -> String {
    const NS_PER_S: f64 = 1e9;
    // Precision loss is irrelevant for a display string.
    #[allow(clippy::cast_precision_loss)]
    let secs = ns.unsigned_abs() as f64 / NS_PER_S;
    if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3_600.0 {
        format!("{:.0}m", secs / 60.0)
    } else if secs < 48.0 * 3_600.0 {
        format!("{:.1}h", secs / 3_600.0)
    } else {
        format!("{:.1}d", secs / 86_400.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    #[test]
    fn span_picks_a_readable_unit() {
        assert_eq!(span(45 * S), "45s");
        assert_eq!(span(-12 * 60 * S), "12m");
        assert_eq!(span(3 * 3_600 * S + 12 * 60 * S), "3.2h");
        assert_eq!(span(108 * 3_600 * S), "4.5d");
    }

    #[test]
    fn time_reasons_read_naturally() {
        let r = RejectReason::TooOld {
            produced_at_ns: 0,
            now_ns: 40 * 3_600 * S,
            max_age_ns: 36 * 3_600 * S,
        };
        assert_eq!(r.to_string(), "too old: produced 40.0h ago, max age 36.0h");
        let r = RejectReason::ProducedInFuture {
            produced_at_ns: 10 * 60 * S,
            now_ns: 0,
        };
        assert_eq!(r.to_string(), "produced 10m in the future");
    }
}
