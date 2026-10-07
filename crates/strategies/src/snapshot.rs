//! The read-only world a strategy decides on, and what it hands back.
//!
//! A [`Snapshot`] borrows everything from the live loop for one decision tick; strategies never
//! mutate it and never perform I/O, so every decision is reproducible from the snapshot alone.

use std::collections::{HashMap, HashSet};

use susq_book::{BookStore, L2Book};
use susq_contracts::v1::{RacePosterior, TargetFile, TargetPosition};
use susq_core::{
    Action, BookSide, ExchangeId, MarketIndex, OrderBundle, Pips, Side, config::FairCfg, side::yes_space,
};
use susq_platform::{ConstraintEval, Relationship};

/// Nanoseconds per second.
pub(crate) const NS_PER_S: i64 = 1_000_000_000;

/// The blended fair value of one exchange (YES probability).
#[derive(Debug, Clone, PartialEq)]
pub struct FairQuote {
    /// Fair YES probability in `[0, 1]`.
    pub p_yes: f64,
    /// When the inputs behind this fair were observed (UTC ns).
    pub observed_at_ns: i64,
    /// `p_yes` at the previous observation (for "real-money moved" checks).
    pub prev_p_yes: Option<f64>,
    /// Provenance, e.g. `"0.8*kalshi+0.2*sim"`.
    pub source: String,
}

impl FairQuote {
    /// Signed move since the previous observation, when both values are finite.
    pub fn moved(&self) -> Option<f64> {
        let prev = self.prev_p_yes?;
        let d = self.p_yes - prev;
        d.is_finite().then_some(d)
    }
}

/// One of our live (pending / sent / acked / open) intents, as the OMS sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentView {
    pub intent_id: String,
    pub strategy: String,
    pub exchange_id: ExchangeId,
    pub side: Side,
    pub action: Action,
    /// Limit price in the order's own side terms.
    pub price: Pips,
    /// Shares still working.
    pub remaining: i64,
    pub created_at_ns: i64,
    pub needle_derived: bool,
}

impl IntentView {
    /// What one share of the order costs on the side it buys (a sell is priced as buying the
    /// complement), in SUSQies.
    pub fn cost_per_share(&self) -> f64 {
        match self.action {
            Action::Buy => self.price.prob(),
            Action::Sell => self.price.complement().prob(),
        }
    }

    /// Cash the unfilled remainder would consume if it filled at the limit.
    pub fn resting_notional(&self) -> f64 {
        self.remaining.max(0) as f64 * self.cost_per_share()
    }

    /// The YES-space level the order rests at.
    pub fn yes_level(&self) -> (BookSide, Pips) {
        yes_space(self.side, self.action, self.price)
    }
}

/// The needle's latest race posteriors plus the watchdog verdict, and what the results pollers
/// say about the feeds behind them (`susq_enr::service::EnrHealth`, only with `--election`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NeedleView {
    /// Watchdog verdict; posteriors must be ignored unless this is true.
    pub healthy: bool,
    /// Posterior generation (increments on every published batch).
    pub epoch: u64,
    /// race_id → posterior.
    pub posteriors: HashMap<String, RacePosterior>,
    /// Races where two results sources disagree beyond tolerance (`EnrHealth.disputed_races`):
    /// the executor refuses them outright and cancels its resting orders on them.
    pub disputed_races: HashSet<String>,
    /// Races whose mapped results sources are all stale (no successful poll for
    /// `[strategies.en] feed_stale_s`, three failures in a row, or never polled): no new buys,
    /// rolls of held legs still happen.
    pub stale_feed_races: HashSet<String>,
}

/// Everything a strategy may read for one decision.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    pub now_ns: i64,
    /// Account value (SUSQies).
    pub equity: f64,
    pub books: &'a BookStore,
    pub book_max_age_ns: i64,
    /// Signed positions: +YES / -NO shares.
    pub positions: &'a HashMap<ExchangeId, i64>,
    /// YES marks per exchange (the account's valuation where we hold, else the book mid).
    pub marks: &'a HashMap<ExchangeId, f64>,
    pub intents: &'a [IntentView],
    pub markets: &'a MarketIndex,
    pub fair: &'a HashMap<ExchangeId, FairQuote>,
    pub fair_cfg: &'a FairCfg,
    pub targets: Option<&'a TargetFile>,
    /// False when the target file is stale or invalid: executors may only reduce exposure.
    pub targets_increase_allowed: bool,
    pub needle: Option<&'a NeedleView>,
    /// Relationships by id.
    pub relationships: &'a HashMap<String, Relationship>,
    /// Current engine violations (realtime + polling).
    pub violations: &'a [ConstraintEval],
    /// Cluster label → current gross exposure as a fraction of its cap (0..=1+), for MM skew.
    pub cluster_utilization: &'a HashMap<String, f64>,
}

impl<'a> Snapshot<'a> {
    /// Whole seconds since the epoch, used in intent ids.
    pub fn now_s(&self) -> i64 {
        self.now_ns.div_euclid(NS_PER_S)
    }

    /// A book safe to decide on (present, clean, uncrossed, young enough).
    pub fn fresh_book(&self, exchange: &ExchangeId) -> Option<&'a L2Book> {
        self.books.fresh(exchange, self.now_ns, self.book_max_age_ns)
    }

    /// Signed position (0 when flat or unknown).
    pub fn position(&self, exchange: &ExchangeId) -> i64 {
        self.positions.get(exchange).copied().unwrap_or(0)
    }

    /// Money at risk on one position: `|qty|` times the mark of the held side (YES at `m`, NO at
    /// `1 - m`); an unmarked position is valued at the full 1.0 payout, the conservative choice.
    pub fn position_value(&self, exchange: &ExchangeId) -> f64 {
        let qty = self.position(exchange);
        if qty == 0 {
            return 0.0;
        }
        let px = match self.marks.get(exchange) {
            Some(m) if qty > 0 => *m,
            Some(m) => 1.0 - *m,
            None => 1.0,
        };
        qty.unsigned_abs() as f64 * px.clamp(0.0, 1.0)
    }

    /// Positions at mark plus every live order at its full opening notional, across every
    /// market: what the account has committed, as the bridge's `total_exposure_pips` counts it.
    pub fn total_exposure(&self) -> f64 {
        let positions: f64 = self.positions.keys().map(|ex| self.position_value(ex)).sum();
        let resting: f64 = self.intents.iter().map(IntentView::resting_notional).sum();
        positions + resting
    }

    /// Whether a fair quote is young enough to trade on and holds a usable probability.
    pub fn is_fair_fresh(&self, quote: &FairQuote) -> bool {
        let max_age = i64::try_from(self.fair_cfg.stale_after_s)
            .unwrap_or(i64::MAX)
            .saturating_mul(NS_PER_S);
        quote.p_yes.is_finite()
            && (0.0..=1.0).contains(&quote.p_yes)
            && self.now_ns.saturating_sub(quote.observed_at_ns) <= max_age
    }

    /// The fair quote for an exchange, only if fresh.
    pub fn fresh_fair(&self, exchange: &ExchangeId) -> Option<&'a FairQuote> {
        self.fair.get(exchange).filter(|q| self.is_fair_fresh(q))
    }

    /// Live intents of one strategy.
    pub fn live_intents<'s>(&'s self, strategy: &'s str) -> impl Iterator<Item = &'a IntentView> + 's {
        self.intents.iter().filter(move |i| i.strategy == strategy)
    }

    /// Whether `strategy` already has a live intent on `exchange`.
    pub fn has_live_intent(&self, strategy: &str, exchange: &ExchangeId) -> bool {
        self.live_intents(strategy).any(|i| &i.exchange_id == exchange)
    }

    /// The target row for an exchange, if a target file is loaded.
    pub fn target_for(&self, exchange: &ExchangeId) -> Option<&'a TargetPosition> {
        self.targets?
            .positions
            .iter()
            .find(|t| t.exchange_id == exchange.as_str())
    }

    /// Whether the target file marks this exchange's leg as locked (never sold or reduced).
    pub fn is_locked(&self, exchange: &ExchangeId) -> bool {
        self.target_for(exchange).is_some_and(|t| t.locked)
    }

    /// Fair quotes in stable (exchange id) order, so decisions are deterministic.
    pub fn fair_sorted(&self) -> Vec<(&'a ExchangeId, &'a FairQuote)> {
        let mut v: Vec<_> = self.fair.iter().collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v
    }
}

/// Ask the OMS to cancel one of our live intents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequest {
    pub intent_id: String,
    pub reason: String,
}

/// A structured summary a strategy wants in the journal (strategies do no I/O; the engine writes it).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Journal method, e.g. `farquote_scan`.
    pub kind: &'static str,
    pub body: serde_json::Value,
}

/// A strategy's output for one tick: new orders, cancels, journal events and human-readable notes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decision {
    pub bundles: Vec<OrderBundle>,
    pub cancels: Vec<CancelRequest>,
    pub events: Vec<Event>,
    pub notes: Vec<String>,
}

impl Decision {
    /// True when the decision asks for nothing (notes and events do not count).
    pub fn is_empty(&self) -> bool {
        self.bundles.is_empty() && self.cancels.is_empty()
    }

    /// Record a note.
    pub(crate) fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }

    /// Queue a journal event.
    pub(crate) fn event(&mut self, kind: &'static str, body: serde_json::Value) {
        self.events.push(Event { kind, body });
    }

    /// Cancel an intent unless a cancel for it is already queued.
    pub(crate) fn cancel(&mut self, intent_id: &str, reason: impl Into<String>) {
        if !self.cancels.iter().any(|c| c.intent_id == intent_id) {
            self.cancels.push(CancelRequest {
                intent_id: intent_id.to_owned(),
                reason: reason.into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{World, intent};

    #[test]
    fn exposure_counts_positions_at_mark_and_orders_at_their_limit() {
        let mut w = World::new(1_000.0);
        w.position("1", 100); // YES at a 0.30 mark: 30
        w.mark("1", 0.30);
        w.position("2", -40); // NO at a 0.75 mark: 40 × 0.25 = 10
        w.mark("2", 0.75);
        w.position("3", -5); // unmarked: the full payout, 5
        w.intents
            .push(intent("mm:1:1", "mm", "1", Side::Yes, Action::Buy, 2000)); // 10 × 0.20 = 2
        w.intents
            .push(intent("mm:2:1", "mm", "2", Side::No, Action::Sell, 2000)); // 10 × 0.80 = 8
        let snap = w.snap();
        assert_eq!(snap.position_value(&ExchangeId::new("1")), 30.0);
        assert_eq!(snap.position_value(&ExchangeId::new("2")), 10.0);
        assert_eq!(snap.position_value(&ExchangeId::new("3")), 5.0);
        assert_eq!(snap.position_value(&ExchangeId::new("9")), 0.0);
        assert_eq!(snap.intents[1].cost_per_share(), 0.8);
        assert_eq!(
            snap.intents[1].yes_level(),
            (BookSide::Bid, Pips::new(8000).expect("valid"))
        );
        assert_eq!(snap.total_exposure(), 55.0);
    }
}
