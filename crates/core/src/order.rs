//! What strategies ask for: priced, sized, time-limited order specs.
//!
//! Strategies emit [`OrderSpec`]s grouped into [`OrderBundle`]s; risk checks them, the OMS turns them
//! into write-ahead intents with deterministic idempotency keys, and the gateway sends them.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    ids::{ExchangeId, MarketId, Qty},
    price::Pips,
    side::{Action, BookSide, Side, yes_space},
};

/// One limit order a strategy wants on the book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderSpec {
    /// Deterministic identity of the decision (e.g. `racer:rel-9:1791475200`). Re-deciding the same
    /// thing yields the same id, so the OMS will never send it twice.
    pub intent_id: String,
    /// Strategy name (`racer`, `sniper`, `mm`, `scenario`, `en`, `smoke`).
    pub strategy: String,
    pub exchange_id: ExchangeId,
    /// Market containing the exchange, when known (for cancel scoping and clusters).
    pub market_id: Option<MarketId>,
    pub side: Side,
    pub action: Action,
    pub quantity: Qty,
    pub price: Pips,
    /// Requested time-to-live; the expiration is further capped by the calendar.
    pub ttl: Duration,
    /// Built on needle posteriors: cancelled when the needle watchdog goes unhealthy.
    pub needle_derived: bool,
    /// Fair value (YES probability) the strategy used, for the glitch cap.
    pub fair_yes: Option<f64>,
}

impl OrderSpec {
    /// Which side of the YES book this order rests on, and at what YES price.
    pub fn yes_space(&self) -> (BookSide, Pips) {
        yes_space(self.side, self.action, self.price)
    }

    /// Signed YES-position change if fully filled.
    pub fn yes_delta(&self) -> i64 {
        self.yes_space().0.yes_delta_sign() * self.quantity.get()
    }

    /// Cash paid per share if filled at the limit. Sells are priced as buying the complement,
    /// which is how the engine canonicalizes an uncovered sell (the conservative case).
    pub fn cost_per_share(&self) -> Pips {
        match self.action {
            Action::Buy => self.price,
            Action::Sell => self.price.complement(),
        }
    }

    /// Worst-case cash outlay (quantity × per-share cost), in SUSQies.
    pub fn max_notional(&self) -> f64 {
        self.cost_per_share().prob() * self.quantity.get() as f64
    }
}

/// Orders that belong together. `atomic` bundles go through `/orders/multi-leg` (all or nothing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBundle {
    pub legs: Vec<OrderSpec>,
    pub atomic: bool,
}

impl OrderBundle {
    /// A single, non-atomic order.
    pub fn single(spec: OrderSpec) -> Self {
        Self {
            legs: vec![spec],
            atomic: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(side: Side, action: Action, price: u32) -> OrderSpec {
        OrderSpec {
            intent_id: "t".into(),
            strategy: "test".into(),
            exchange_id: ExchangeId::new("1"),
            market_id: None,
            side,
            action,
            quantity: Qty(10),
            price: Pips::new(price).expect("valid"),
            ttl: Duration::from_secs(30),
            needle_derived: false,
            fair_yes: None,
        }
    }

    #[test]
    fn deltas_and_costs() {
        let b = spec(Side::No, Action::Buy, 3000);
        assert_eq!(b.yes_delta(), -10);
        assert!((b.max_notional() - 3.0).abs() < 1e-9);
        let s = spec(Side::Yes, Action::Sell, 3000);
        assert_eq!(s.yes_delta(), -10);
        assert!((s.max_notional() - 7.0).abs() < 1e-9);
    }
}
