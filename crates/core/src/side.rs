//! Sides, actions and the YES-space view of an order.
//!
//! The platform keeps one YES-normalized book per exchange: `sell yes q@p` ≡ `buy no q@(1-p)`.
//! [`yes_space`] maps any `(side, action, price)` onto the bid or ask of that single book.

use std::fmt;

use serde::{Deserialize, Serialize};
use susq_contracts::v1;

use crate::price::Pips;

/// Outcome side of a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

/// Order action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Buy,
    Sell,
}

/// Which side of the YES book an order rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BookSide {
    Bid,
    Ask,
}

impl Side {
    /// Lowercase wire form (`"yes"` / `"no"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Side::Yes => "yes",
            Side::No => "no",
        }
    }

    /// The other outcome.
    pub const fn opposite(self) -> Side {
        match self {
            Side::Yes => Side::No,
            Side::No => Side::Yes,
        }
    }

    /// Parse the wire form, case-insensitively.
    pub fn parse(s: &str) -> Option<Side> {
        match s.to_ascii_lowercase().as_str() {
            "yes" => Some(Side::Yes),
            "no" => Some(Side::No),
            _ => None,
        }
    }

    /// Position sign of holding this side (+YES / -NO).
    pub const fn sign(self) -> i64 {
        match self {
            Side::Yes => 1,
            Side::No => -1,
        }
    }
}

impl Action {
    /// Lowercase wire form (`"buy"` / `"sell"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::Buy => "buy",
            Action::Sell => "sell",
        }
    }

    /// Parse the wire form, case-insensitively.
    pub fn parse(s: &str) -> Option<Action> {
        match s.to_ascii_lowercase().as_str() {
            "buy" => Some(Action::Buy),
            "sell" => Some(Action::Sell),
            _ => None,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BookSide {
    /// Signed YES-position change per share when an order on this side fills.
    pub const fn yes_delta_sign(self) -> i64 {
        match self {
            BookSide::Bid => 1,
            BookSide::Ask => -1,
        }
    }

    /// The side this one trades against.
    pub const fn opposite(self) -> BookSide {
        match self {
            BookSide::Bid => BookSide::Ask,
            BookSide::Ask => BookSide::Bid,
        }
    }
}

/// Map an order onto the single YES book: which side it rests on and at what YES price.
pub const fn yes_space(side: Side, action: Action, price: Pips) -> (BookSide, Pips) {
    match (side, action) {
        (Side::Yes, Action::Buy) => (BookSide::Bid, price),
        (Side::Yes, Action::Sell) => (BookSide::Ask, price),
        (Side::No, Action::Buy) => (BookSide::Ask, price.complement()),
        (Side::No, Action::Sell) => (BookSide::Bid, price.complement()),
    }
}

impl From<Side> for v1::Side {
    fn from(s: Side) -> Self {
        match s {
            Side::Yes => v1::Side::Yes,
            Side::No => v1::Side::No,
        }
    }
}

impl From<Action> for v1::Action {
    fn from(a: Action) -> Self {
        match a {
            Action::Buy => v1::Action::Buy,
            Action::Sell => v1::Action::Sell,
        }
    }
}

impl TryFrom<v1::Side> for Side {
    type Error = ();
    fn try_from(s: v1::Side) -> Result<Self, ()> {
        match s {
            v1::Side::Yes => Ok(Side::Yes),
            v1::Side::No => Ok(Side::No),
            v1::Side::Unspecified => Err(()),
        }
    }
}

impl TryFrom<v1::Action> for Action {
    type Error = ();
    fn try_from(a: v1::Action) -> Result<Self, ()> {
        match a {
            v1::Action::Buy => Ok(Action::Buy),
            v1::Action::Sell => Ok(Action::Sell),
            v1::Action::Unspecified => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sell_yes_equals_buy_no_in_yes_space() {
        let p = Pips::new(3000).expect("valid");
        assert_eq!(
            yes_space(Side::Yes, Action::Sell, p),
            yes_space(Side::No, Action::Buy, p.complement())
        );
        assert_eq!(
            yes_space(Side::Yes, Action::Buy, p),
            yes_space(Side::No, Action::Sell, p.complement())
        );
        assert_eq!(
            yes_space(Side::No, Action::Buy, p),
            (BookSide::Ask, Pips::new(7000).expect("valid"))
        );
    }

    #[test]
    fn wire_forms() {
        assert_eq!(Side::parse("YES"), Some(Side::Yes));
        assert_eq!(Action::parse("sell"), Some(Action::Sell));
        assert_eq!(Side::parse("maybe"), None);
        assert_eq!(serde_json::to_string(&Side::No).expect("json"), "\"no\"");
        assert_eq!(v1::Side::from(Side::No), v1::Side::No);
        assert_eq!(Side::try_from(v1::Side::Yes), Ok(Side::Yes));
    }
}
