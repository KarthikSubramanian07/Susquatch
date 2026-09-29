//! Identifier newtypes so an exchange id can never be passed where a market id is expected.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap a platform id.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// Borrow the raw id.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(
    /// Tradable contract id (the API's "exchange"), a numeric string on the wire.
    ExchangeId
);
string_id!(
    /// Market (container of one or more exchanges) id.
    MarketId
);
string_id!(
    /// Tournament UUID (`GET /tournaments/{slug}` → `id`). Sent on every call.
    TournamentId
);

/// Platform order id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(pub i64);

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Share quantity. Order quantities are positive; positions are signed (+YES / -NO).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Qty(pub i64);

/// Largest order quantity the engine accepts (signed 32-bit).
pub const MAX_ORDER_QTY: i64 = i32::MAX as i64;

impl Qty {
    /// Zero shares.
    pub const ZERO: Qty = Qty(0);

    /// A valid order quantity in `1..=i32::MAX`.
    pub fn order(q: i64) -> Option<Qty> {
        (1..=MAX_ORDER_QTY).contains(&q).then_some(Qty(q))
    }

    /// Raw value.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// Absolute size.
    pub const fn abs(self) -> i64 {
        self.0.abs()
    }

    /// Overflow-checked addition.
    pub fn checked_add(self, other: Qty) -> Option<Qty> {
        self.0.checked_add(other.0).map(Qty)
    }
}

impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_qty_bounds() {
        assert_eq!(Qty::order(0), None);
        assert_eq!(Qty::order(-3), None);
        assert_eq!(Qty::order(1), Some(Qty(1)));
        assert_eq!(Qty::order(MAX_ORDER_QTY + 1), None);
    }

    #[test]
    fn ids_are_transparent_on_the_wire() {
        let e = ExchangeId::new("381");
        assert_eq!(serde_json::to_string(&e).expect("json"), "\"381\"");
        assert_eq!(e.to_string(), "381");
    }
}
