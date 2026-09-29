//! Shared construction of [`OrderSpec`]s so every strategy fills ids, ttl and market ids the same way.

use std::time::Duration;

use susq_core::{Action, ExchangeId, OrderSpec, Pips, Qty, Side};

use crate::snapshot::Snapshot;

/// A priced, sized order before it gets its identity.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Draft {
    pub exchange: ExchangeId,
    pub side: Side,
    pub action: Action,
    pub qty: i64,
    pub price: Pips,
    pub fair_yes: Option<f64>,
}

/// Deterministic intent id: `"{strategy}:{key}:{now_s}"`.
pub(crate) fn intent_id(strategy: &str, key: &str, now_s: i64) -> String {
    format!("{strategy}:{key}:{now_s}")
}

impl Draft {
    /// Turn the draft into an [`OrderSpec`], or `None` when the quantity is not a valid order size.
    pub(crate) fn into_spec(
        self,
        snap: &Snapshot<'_>,
        strategy: &str,
        key: &str,
        ttl_s: u64,
        needle_derived: bool,
    ) -> Option<OrderSpec> {
        let quantity = Qty::order(self.qty)?;
        Some(OrderSpec {
            intent_id: intent_id(strategy, key, snap.now_s()),
            strategy: strategy.to_owned(),
            market_id: snap.markets.market_of(&self.exchange),
            exchange_id: self.exchange,
            side: self.side,
            action: self.action,
            quantity,
            price: self.price,
            ttl: Duration::from_secs(ttl_s),
            needle_derived,
            fair_yes: self.fair_yes,
        })
    }
}

/// Largest whole-share quantity whose cost at `price` stays within `budget` SUSQies (0 if none).
pub(crate) fn shares_for_budget(budget: f64, price: Pips) -> i64 {
    shares_for_cost(budget, price.prob())
}

/// Largest whole-share quantity whose cost at `per_share` SUSQies each stays within `budget`.
pub(crate) fn shares_for_cost(budget: f64, per_share: f64) -> i64 {
    if !(budget.is_finite() && budget > 0.0 && per_share.is_finite() && per_share > 0.0) {
        return 0;
    }
    // A tiny epsilon keeps exact budgets (e.g. 1000 × 0.5 = 500) from flooring one share short.
    let q = (budget / per_share + 1e-9).floor();
    if q >= i64::MAX as f64 { i64::MAX } else { q as i64 }
}
