//! Signed positions (+YES / −NO shares) per exchange.
//!
//! REST (`/portfolio/positions`) is the truth and replaces everything on each refresh; fills from
//! acks are applied optimistically in between so risk never under-counts exposure.

use std::collections::HashMap;

use susq_core::ExchangeId;
use susq_platform::Positions;

use crate::oms::FillDelta;

/// Position and mark per exchange.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PositionTracker {
    qty: HashMap<ExchangeId, i64>,
    /// Tournament valuation price (YES space) from the last REST refresh.
    marks: HashMap<ExchangeId, f64>,
    refreshed_at_ns: Option<i64>,
}

impl PositionTracker {
    /// Empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace everything with the REST view.
    pub fn set_from_rest(&mut self, p: &Positions, now_ns: i64) {
        self.qty.clear();
        self.marks.clear();
        for pos in p.positions.iter().filter(|p| !p.settled) {
            let q = pos.quantity.round() as i64;
            if q != 0 {
                self.qty.insert(pos.exchange_id.clone(), q);
            }
            if let Some(m) = pos.current_price {
                self.marks.insert(pos.exchange_id.clone(), m);
            }
        }
        self.refreshed_at_ns = Some(now_ns);
    }

    /// Apply a fill reported by an ack.
    pub fn apply(&mut self, d: &FillDelta) {
        let e = self.qty.entry(d.exchange_id.clone()).or_default();
        *e += d.yes_delta;
        if *e == 0 {
            self.qty.remove(&d.exchange_id);
        }
    }

    /// Signed position.
    pub fn get(&self, ex: &ExchangeId) -> i64 {
        self.qty.get(ex).copied().unwrap_or(0)
    }

    /// All non-zero positions.
    pub fn map(&self) -> &HashMap<ExchangeId, i64> {
        &self.qty
    }

    /// Last REST mark (YES space).
    pub fn mark(&self, ex: &ExchangeId) -> Option<f64> {
        self.marks.get(ex).copied()
    }

    /// When REST last refreshed the view.
    pub fn refreshed_at_ns(&self) -> Option<i64> {
        self.refreshed_at_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_platform::Position;

    #[test]
    fn rest_replaces_and_fills_apply() {
        let mut t = PositionTracker::new();
        let ex = ExchangeId::new("1");
        t.apply(&FillDelta {
            exchange_id: ex.clone(),
            yes_delta: 5,
        });
        assert_eq!(t.get(&ex), 5);
        let rest = Positions {
            positions: vec![Position {
                exchange_id: ex.clone(),
                market_id: None,
                market_title: String::new(),
                option: None,
                settled: false,
                quantity: -3.0,
                avg_cost: 0.4,
                current_price: Some(0.55),
                market_value: 0.0,
                cost_basis: 0.0,
                unrealized_pnl: 0.0,
                money_earned: 0.0,
                lots: vec![],
            }],
            summary: Default::default(),
        };
        t.set_from_rest(&rest, 10);
        assert_eq!(t.get(&ex), -3);
        assert_eq!(t.mark(&ex), Some(0.55));
        t.apply(&FillDelta {
            exchange_id: ex.clone(),
            yes_delta: 3,
        });
        assert!(t.map().is_empty());
    }
}
