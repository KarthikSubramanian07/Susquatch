//! Reader/validator for the versioned target files the Python scheduler hands to `rust-live`.
//!
//! # Contract
//!
//! Python writes `targets/targets-<utc>-<hash8>.json` (a `susq.v1.TargetFile` in proto3 JSON) and then
//! atomically swaps `targets/current.json`, a `susq.v1.TargetPointer`:
//! `{ header, file, contentSha256 }`. `file` must be a single plain file name inside the targets
//! directory (no separators, `.`/`..`, or absolute paths). `current.json` may instead be a copy of, or
//! a symlink to, a `TargetFile` itself; it is treated as a pointer when it has a `file` field, and
//! then there is no pointer hash to compare. (proto3 JSON omits an empty `file`, so such a pointer
//! is read as a bare `TargetFile` and refused by its checks.)
//!
//! A file is adopted only if all of these hold, each failure being a distinct [`RejectReason`]:
//!
//! * pointer and file parse, both headers exist and carry [`susq_contracts::SCHEMA_VERSION`];
//! * the pointer's `content_sha256` equals the file's, and the file's equals
//!   [`susq_contracts::target_content_sha256`] recomputed over the parsed content;
//! * `tournament_slug` matches the configured tournament;
//! * time: `produced_at_ns` is at most [`ReaderConfig::future_skew`] ahead of now,
//!   `valid_until_ns > now`, and `now - produced_at_ns <= max_age`;
//! * content ([`ContentError`]): every fair value has a non-empty, unique `exchange_id`, finite
//!   `p_yes`/`p_lo`/`p_hi` in \[0, 1\] with `p_lo <= p_yes <= p_hi`; every target position has a
//!   non-empty, unique `exchange_id`, `max_entry_price` finite in (0, 1) and `target_qty` within `i32`;
//!   every `max_gross_frac` is finite in \[0, 1\]; tourney probabilities are finite in \[0, 1\] and its
//!   thresholds finite.
//!
//! There is no "no band" encoding: a fair value without a meaningful band must be written with
//! `p_lo = p_yes = p_hi`. A band of `0, 0` around a non-zero `p_yes` is refused, because proto3 omits
//! zero fields and a forgotten band would otherwise pass silently.
//!
//! # Fail-safe rule
//!
//! Stale or malformed targets never increase exposure. On any rejection, or when the last good file
//! itself expires or exceeds the maximum age, [`TargetsReader`] keeps serving the last good
//! [`Targets`] (so strategies can still reduce and hold) but
//! [`TargetsReader::increase_exposure_allowed`] turns false. It turns true again only when a fresh,
//! valid file loads. With nothing ever loaded, there are no targets and increases are blocked.
//!
//! ```no_run
//! use std::time::Duration;
//! use susq_targets::{ReaderConfig, RefreshOutcome, TargetsReader};
//!
//! let cfg = ReaderConfig::new("targets", Duration::from_secs(36 * 3600), "midterm-elections");
//! let mut reader = TargetsReader::new(cfg);
//! let now_ns = 1_791_478_800_000_000_000;
//! if let RefreshOutcome::Rejected(reason) = reader.refresh(now_ns) {
//!     eprintln!("targets rejected: {reason}");
//! }
//! let may_add = reader.increase_exposure_allowed();
//! let target = reader.current().and_then(|t| t.target_for("210"));
//! # let _ = (may_add, target);
//! ```

mod error;
pub mod fair;
mod reader;
mod source;
mod targets;
mod validate;

pub use error::{ContentError, Document, RejectReason};
pub use fair::{FairOutcome, FairReader, FairReject, FairSnapshot};
pub use reader::{ReaderConfig, RefreshOutcome, TargetsReader};
pub use targets::Targets;
