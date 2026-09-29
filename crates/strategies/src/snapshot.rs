//! The read-only world a strategy decides on, and what it hands back.
//!
//! A [`Snapshot`] borrows everything from the live loop for one decision tick; strategies never
//! mutate it and never perform I/O, so every decision is reproducible from the snapshot alone.

use std::collections::HashMap;

use susq_book::{BookStore, L2Book};
use susq_contracts::v1::{RacePosterior, TargetFile, TargetPosition};
use susq_core::{Action, ExchangeId, MarketIndex, OrderBundle, Pips, Side, config::FairCfg};
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

/// The needle's latest race posteriors plus the watchdog verdict.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NeedleView {
    /// Watchdog verdict; posteriors must be ignored unless this is true.
    pub healthy: bool,
    /// Posterior generation (increments on every published batch).
    pub epoch: u64,
    /// race_id → posterior.
    pub posteriors: HashMap<String, RacePosterior>,
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

/// A strategy's output for one tick: new orders, cancels, and human-readable notes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decision {
    pub bundles: Vec<OrderBundle>,
    pub cancels: Vec<CancelRequest>,
    pub notes: Vec<String>,
}

impl Decision {
    /// True when the decision asks for nothing (notes do not count).
    pub fn is_empty(&self) -> bool {
        self.bundles.is_empty() && self.cancels.is_empty()
    }

    /// Record a note.
    pub(crate) fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
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
