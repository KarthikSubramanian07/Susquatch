//! The order state machine.
//!
//! ```text
//! Pending ──send──► Sent ──ack──► Acked ──► PartiallyFilled ──► Filled
//!    │                │  │          │  │            │
//!    │                │  └─unknown─► Unknown ─(resend same key / reconcile)─► Acked | Rejected | Expired ...
//!    │                └──4xx──► Rejected           └────────► Cancelled | Expired
//!    └──abandon──► Cancelled
//! ```
//! Terminal states never change again. Illegal transitions are refused (and counted by the OMS),
//! never panicked on.

use susq_contracts::v1::IntentState;

/// Lifecycle of one order intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderState {
    /// Written ahead, not yet on the wire.
    Pending,
    /// Request on the wire, outcome not yet known.
    Sent,
    /// Accepted and resting.
    Acked,
    /// Resting with some quantity filled.
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
    /// Terminal 4xx.
    Rejected,
    /// The engine may or may not have it (502, timeout, retries exhausted): resolve before acting.
    Unknown,
}

impl OrderState {
    /// No further transitions.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Expired | Self::Rejected
        )
    }

    /// Could be resting on the book (or about to be).
    pub const fn is_live(self) -> bool {
        !self.is_terminal()
    }

    /// Whether `self → to` is allowed.
    pub fn can_transition(self, to: OrderState) -> bool {
        use OrderState::*;
        match self {
            Pending => matches!(to, Sent | Cancelled | Rejected),
            Sent => matches!(
                to,
                Acked | PartiallyFilled | Filled | Cancelled | Rejected | Unknown
            ),
            Unknown => matches!(
                to,
                Sent | Acked | PartiallyFilled | Filled | Cancelled | Rejected | Expired
            ),
            Acked => matches!(to, PartiallyFilled | Filled | Cancelled | Expired),
            PartiallyFilled => matches!(to, PartiallyFilled | Filled | Cancelled | Expired),
            Filled | Cancelled | Expired | Rejected => false,
        }
    }

    /// Persisted form. Terminal fills/cancels/expiries are all `DONE`; the detail goes in
    /// `last_error_code` (see [`OrderState::done_reason`]).
    pub const fn to_proto(self) -> IntentState {
        match self {
            Self::Pending => IntentState::Pending,
            Self::Sent => IntentState::Sent,
            Self::Acked | Self::PartiallyFilled => IntentState::Acked,
            Self::Unknown => IntentState::Unknown,
            Self::Rejected => IntentState::Rejected,
            Self::Filled | Self::Cancelled | Self::Expired => IntentState::Done,
        }
    }

    /// Close reason stored with a `DONE` intent.
    pub const fn done_reason(self) -> Option<&'static str> {
        match self {
            Self::Filled => Some("FILLED"),
            Self::Cancelled => Some("CANCELLED"),
            Self::Expired => Some("EXPIRED"),
            _ => None,
        }
    }

    /// Rebuild from the persisted form.
    pub fn from_proto(state: i32, last_error_code: &str) -> OrderState {
        match IntentState::try_from(state) {
            Ok(IntentState::Pending) => Self::Pending,
            Ok(IntentState::Sent) => Self::Sent,
            Ok(IntentState::Acked) => Self::Acked,
            Ok(IntentState::Rejected) => Self::Rejected,
            Ok(IntentState::Done) => match last_error_code {
                "CANCELLED" => Self::Cancelled,
                "EXPIRED" => Self::Expired,
                _ => Self::Filled,
            },
            // UNKNOWN, UNSPECIFIED and anything newer: treat as unresolved (fail safe).
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use OrderState::*;

    const ALL: [OrderState; 9] = [
        Pending,
        Sent,
        Acked,
        PartiallyFilled,
        Filled,
        Cancelled,
        Expired,
        Rejected,
        Unknown,
    ];

    #[test]
    fn terminal_states_are_final() {
        for from in ALL.into_iter().filter(|s| s.is_terminal()) {
            for to in ALL {
                assert!(!from.can_transition(to), "{from:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn nothing_skips_the_wire() {
        // An order can only become acked/filled after it was sent (or while unknown).
        for to in [Acked, PartiallyFilled, Filled, Expired] {
            assert!(!Pending.can_transition(to));
        }
    }

    #[test]
    fn proto_round_trip() {
        for s in ALL {
            let back = OrderState::from_proto(s.to_proto() as i32, s.done_reason().unwrap_or(""));
            let expected = if s == PartiallyFilled { Acked } else { s };
            assert_eq!(back, expected);
        }
        assert_eq!(OrderState::from_proto(99, ""), Unknown);
    }
}
