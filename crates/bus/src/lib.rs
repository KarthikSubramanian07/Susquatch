//! NATS client for rust-live: publish `enr.county_results`, subscribe `needle.*`, and decide
//! whether needle numbers may be traded on.
//!
//! # Modules
//!
//! * [`client`]: [`Bus`], the NATS connection (stamped headers, strictly increasing `seq` per
//!   stream, decoded subscriptions, background reconnect forever).
//! * [`message`]: pure payload decoding ([`NeedleMessage`], [`decode_kill`]).
//! * [`validate`]: content checks and the rolling [`ErrorRate`].
//! * [`watchdog`]: the pure needle health state machine.
//! * [`monitor`]: [`NeedleMonitor`], which ties them together into a [`NeedleSnapshot`] stream.
//!
//! # Fail-safe rules
//!
//! 1. **Unhealthy until proven healthy.** The needle starts untrusted and becomes trusted only
//!    after `needle.resume_after_healthy` consecutive healthy messages.
//! 2. **Silence is failure.** If no healthy message arrives on any needle stream for longer than
//!    `needle.heartbeat_timeout_s`, the needle is unhealthy. Heartbeats alone keep an idle needle
//!    alive; a flowing posterior stream is proof of life too.
//! 3. **No gaps, no reorders, no old numbers.** A `seq` gap, duplicate or reorder, a bad header,
//!    a foreign producer, a message older than the timeout, or one dated in the future is an
//!    unhealthy event. So is a degraded heartbeat status.
//! 4. **Malformed means dropped.** Undecodable or invalid messages (non-finite values,
//!    probabilities outside `[0, 1]`, unknown race ids, ...) are dropped whole, logged, counted in
//!    `errors_per_min` (risk trips its breaker at `risk.bus_error_breaker_per_min`), and count as
//!    unhealthy events.
//! 5. **Outages are explicit.** Every healthy -> unhealthy transition bumps
//!    [`NeedleSnapshot::epoch`] and clears stored numbers; the executor cancels needle-derived
//!    orders once per epoch change. Numbers are only reachable through the `actionable_*`
//!    accessors, which return `None` when unhealthy, when the snapshot itself is older than the
//!    timeout (a dead monitor cannot leave a stale "healthy" behind), and per entry when that
//!    number is older than the timeout.
//! 6. **Kills are never dropped.** `ops.kill` is honored even with a missing or wrong header, and
//!    an undecodable kill payload still yields a kill ([`decode_kill`]).
//! 7. **NATS down is survivable.** Connecting never fails because the server is down; publishing
//!    is bounded by a timeout; subscriptions are restored on reconnect.

pub mod client;
pub mod message;
pub mod monitor;
pub mod validate;
pub mod watchdog;

pub use client::{Bus, BusError};
pub use message::{NeedleMessage, decode_kill};
pub use monitor::{MonitorCfg, NeedleMonitor, NeedleSnapshot, NeedleState, Stamped};
pub use validate::{ErrorRate, ValidationError};
pub use watchdog::{NeedleStream, Transition, UnhealthyReason, Watchdog, WatchdogCfg};

/// NATS subjects (binary protobuf payloads; see `contracts/proto/susq/v1/bus.proto`).
pub mod subjects {
    /// `CountyResultsBatch`, rust-live (enr) -> needle.
    pub const ENR_COUNTY_RESULTS: &str = "enr.county_results";
    /// `RacePosteriors`, needle -> rust-live.
    pub const NEEDLE_RACE_POSTERIORS: &str = "needle.race_posteriors";
    /// `NationalShift`, needle -> rust-live.
    pub const NEEDLE_NATIONAL_SHIFT: &str = "needle.national_shift";
    /// `Heartbeat`, needle -> rust-live, every <= 2 s.
    pub const NEEDLE_HEARTBEAT: &str = "needle.heartbeat";
    /// `Kill`, any -> all.
    pub const OPS_KILL: &str = "ops.kill";
    /// Every subject the needle publishes on.
    pub const NEEDLE: [&str; 3] = [NEEDLE_RACE_POSTERIORS, NEEDLE_NATIONAL_SHIFT, NEEDLE_HEARTBEAT];
}
