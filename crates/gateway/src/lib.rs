//! Adaptive rate limiter, retry policy, lease + kill enforcement in front of the platform.
//!
//! [`Gateway`] is the only write path. Its gates, in order:
//!
//! 1. [`kill`]: `ops/KILL`, the in-process flag, NATS `ops.kill`.
//! 2. [`lease`]: the single-trader `TraderLease` (shared with the Python bridge).
//! 3. [`mode`]: paper by default; live needs `SUSQ_MODE=live` and `ops/GO_LIVE` = "go live".
//! 4. Risk freeze, time gate and the risk pre-check (`susq-risk`).
//! 5. Write-ahead intent (`susq-oms`) and journal, then the send with [`retry`] (same key,
//!    documented-safe codes only, backoff with jitter, `Retry-After`, ≥ 90 s for in-flight).
//!
//! Underneath, [`limited::RateLimited`] spends [`limiter`] tokens (AIMD on 429, reserved write
//! budget) and [`paper::PaperBroker`] replaces the platform for writes in paper mode.

pub mod gateway;
pub mod kill;
pub mod lease;
pub mod limited;
pub mod limiter;
pub mod mode;
pub mod paper;
pub mod retry;

pub use gateway::{Blocked, Gateway, GatewayDeps, SendOutcome};
pub use kill::KillSwitch;
pub use lease::{Lease, LeaseConfig, LeaseStatus};
pub use limited::RateLimited;
pub use limiter::{LimiterConfig, RateLimiter, RequestClass};
pub use mode::{go_live_armed, resolve_mode};
pub use paper::PaperBroker;
pub use retry::{Jitter, RetryPolicy};
