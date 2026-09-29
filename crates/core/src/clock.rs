//! Time source abstraction so every time-dependent decision is testable.

use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};

/// Wall-clock source in UTC nanoseconds.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Nanoseconds since the Unix epoch.
    fn now_ns(&self) -> i64;

    /// Current time as a `DateTime`.
    fn now(&self) -> DateTime<Utc> {
        Utc.timestamp_nanos(self.now_ns())
    }
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        to_ns(Utc::now())
    }
}

/// A clock tests move by hand. Clones share the same time.
#[derive(Debug, Clone, Default)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    /// Start at `ns`.
    pub fn at_ns(ns: i64) -> Self {
        Self(Arc::new(AtomicI64::new(ns)))
    }

    /// Start at `t`.
    pub fn at(t: DateTime<Utc>) -> Self {
        Self::at_ns(to_ns(t))
    }

    /// Jump to `ns`.
    pub fn set_ns(&self, ns: i64) {
        self.0.store(ns, Ordering::SeqCst);
    }

    /// Move forward by `d`.
    pub fn advance(&self, d: Duration) {
        let step = i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
        self.0.fetch_add(step, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Convert a `DateTime` to UTC nanoseconds (saturating outside the representable ~1677..2262).
pub fn to_ns(t: DateTime<Utc>) -> i64 {
    t.timestamp_nanos_opt()
        .unwrap_or(if t.timestamp() < 0 { i64::MIN } else { i64::MAX })
}

/// Convert UTC nanoseconds to a `DateTime`.
pub fn from_ns(ns: i64) -> DateTime<Utc> {
    Utc.timestamp_nanos(ns)
}

/// Nanoseconds in `d`, saturating.
pub fn duration_ns(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_is_shared() {
        let c = ManualClock::at_ns(1_000);
        let d = c.clone();
        c.advance(Duration::from_nanos(500));
        assert_eq!(d.now_ns(), 1_500);
        d.set_ns(7);
        assert_eq!(c.now_ns(), 7);
    }

    #[test]
    fn system_clock_is_after_2026() {
        assert!(SystemClock.now().timestamp() > 1_780_000_000);
    }

    #[test]
    fn ns_round_trip() {
        let t: DateTime<Utc> = "2026-10-01T16:00:00Z".parse().expect("ts");
        assert_eq!(from_ns(to_ns(t)), t);
    }
}
