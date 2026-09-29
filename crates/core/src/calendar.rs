//! The fixed contest calendar and the hard time gate.
//!
//! No order may be placed at or after `last_order` (16:55Z on Nov 4) and every order's expiration is
//! capped there, so a dead process can never leave orders resting into the 17:00Z lock.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where we are relative to the contest window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradingWindow {
    /// Before trading opens.
    BeforeOpen,
    /// Orders may be placed.
    Open,
    /// At or after the last-order gate: cancel-only.
    Closed,
}

/// Contest open / last order / lock instants (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Calendar {
    pub open_utc: DateTime<Utc>,
    pub lock_utc: DateTime<Utc>,
    pub last_order_utc: DateTime<Utc>,
}

impl Calendar {
    /// Which window `now` falls in.
    pub fn window(&self, now: DateTime<Utc>) -> TradingWindow {
        if now < self.open_utc {
            TradingWindow::BeforeOpen
        } else if now < self.last_order_utc {
            TradingWindow::Open
        } else {
            TradingWindow::Closed
        }
    }

    /// True iff a new order may be sent at `now`.
    pub fn may_place(&self, now: DateTime<Utc>) -> bool {
        self.window(now) == TradingWindow::Open
    }

    /// Expiration for an order sent at `now` with time-to-live `ttl`, truncated to whole seconds
    /// (the idempotency payload uses unix seconds) and capped at `last_order_utc`.
    /// `None` if that leaves no time in the future, i.e. the order must not be sent.
    pub fn order_expiration(&self, now: DateTime<Utc>, ttl: Duration) -> Option<DateTime<Utc>> {
        let ttl = chrono::Duration::from_std(ttl).ok()?;
        let desired = now.checked_add_signed(ttl)?.min(self.last_order_utc);
        let secs = DateTime::<Utc>::from_timestamp(desired.timestamp(), 0)?;
        (secs > now).then_some(secs)
    }

    /// Consistency check: open < last_order <= lock.
    pub fn validate(&self) -> Result<(), String> {
        if self.open_utc >= self.last_order_utc || self.last_order_utc > self.lock_utc {
            return Err(format!(
                "calendar must satisfy open < last_order <= lock, got {self:?}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal() -> Calendar {
        Calendar {
            open_utc: t("2026-10-01T16:00:00Z"),
            lock_utc: t("2026-11-04T17:00:00Z"),
            last_order_utc: t("2026-11-04T16:55:00Z"),
        }
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().expect("ts")
    }

    #[test]
    fn windows() {
        let c = cal();
        assert!(c.validate().is_ok());
        assert_eq!(c.window(t("2026-10-01T15:59:59Z")), TradingWindow::BeforeOpen);
        assert_eq!(c.window(t("2026-10-01T16:00:00Z")), TradingWindow::Open);
        assert_eq!(c.window(t("2026-11-04T16:54:59Z")), TradingWindow::Open);
        assert_eq!(c.window(t("2026-11-04T16:55:00Z")), TradingWindow::Closed);
        assert!(!c.may_place(t("2026-11-04T16:55:00Z")));
    }

    #[test]
    fn expiration_is_capped_and_whole_seconds() {
        let c = cal();
        let e = c.order_expiration(t("2026-10-02T00:00:00.700Z"), Duration::from_secs(30));
        assert_eq!(e, Some(t("2026-10-02T00:00:30Z")));
        let late = c.order_expiration(t("2026-11-04T16:54:00Z"), Duration::from_secs(300));
        assert_eq!(late, Some(t("2026-11-04T16:55:00Z")));
        assert_eq!(
            c.order_expiration(t("2026-11-04T16:55:00Z"), Duration::from_secs(300)),
            None
        );
        // A sub-second TTL that truncates to "now" is refused.
        assert_eq!(
            c.order_expiration(t("2026-10-02T00:00:00Z"), Duration::from_millis(500)),
            None
        );
    }

    #[test]
    fn invalid_calendar_is_rejected() {
        let mut c = cal();
        c.last_order_utc = t("2026-11-04T17:01:00Z");
        assert!(c.validate().is_err());
    }
}
