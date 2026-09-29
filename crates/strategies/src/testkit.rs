//! Hand-built worlds for strategy tests: own every piece of state a [`Snapshot`] borrows.

use std::collections::HashMap;

use proptest::prelude::*;
use susq_book::{BookStore, L2Book, Level};
use susq_contracts::v1::{MarketMap, MarketRule, RacePosterior, TargetFile, TargetPosition};
use susq_core::{Action, ExchangeId, MarketIndex, Pips, Side, config::FairCfg};
use susq_platform::{ConstraintEval, NodeRole, Relationship, RelationshipNode, RelationshipType};

use crate::snapshot::{FairQuote, IntentView, NS_PER_S, NeedleView, Snapshot};

/// 2026-10-08T00:00:00Z.
pub const NOW_S: i64 = 1_791_417_600;

/// A valid price from raw pips.
pub fn p(raw: u32) -> Pips {
    Pips::new(raw).expect("valid test price")
}

/// Everything a snapshot borrows, owned.
#[derive(Debug, Clone)]
pub struct World {
    pub now_ns: i64,
    pub equity: f64,
    pub books: BookStore,
    pub book_max_age_ns: i64,
    pub positions: HashMap<ExchangeId, i64>,
    pub intents: Vec<IntentView>,
    rules: Vec<MarketRule>,
    markets: MarketIndex,
    pub fair: HashMap<ExchangeId, FairQuote>,
    pub fair_cfg: FairCfg,
    pub targets: Option<TargetFile>,
    pub targets_increase_allowed: bool,
    pub needle: Option<NeedleView>,
    pub relationships: HashMap<String, Relationship>,
    pub violations: Vec<ConstraintEval>,
    pub cluster_utilization: HashMap<String, f64>,
}

impl World {
    /// Empty world at [`NOW_S`] with 10 s book freshness and a 60 s fair staleness gate.
    pub fn new(equity: f64) -> Self {
        Self {
            now_ns: NOW_S * NS_PER_S,
            equity,
            books: BookStore::new(),
            book_max_age_ns: 10 * NS_PER_S,
            positions: HashMap::new(),
            intents: Vec::new(),
            rules: Vec::new(),
            markets: MarketIndex::default(),
            fair: HashMap::new(),
            fair_cfg: FairCfg {
                kalshi_weight: 0.8,
                sim_weight: 0.2,
                stale_after_s: 60,
                mapping_error_pts: 25.0,
            },
            targets: None,
            targets_increase_allowed: true,
            needle: None,
            relationships: HashMap::new(),
            violations: Vec::new(),
            cluster_utilization: HashMap::new(),
        }
    }

    /// Borrow the world as a decision snapshot.
    pub fn snap(&self) -> Snapshot<'_> {
        Snapshot {
            now_ns: self.now_ns,
            equity: self.equity,
            books: &self.books,
            book_max_age_ns: self.book_max_age_ns,
            positions: &self.positions,
            intents: &self.intents,
            markets: &self.markets,
            fair: &self.fair,
            fair_cfg: &self.fair_cfg,
            targets: self.targets.as_ref(),
            targets_increase_allowed: self.targets_increase_allowed,
            needle: self.needle.as_ref(),
            relationships: &self.relationships,
            violations: &self.violations,
            cluster_utilization: &self.cluster_utilization,
        }
    }

    /// Add or replace the market rule of `ex`.
    pub fn rule(&mut self, ex: &str, market: &str, race: &str, party: &str, verified: bool) {
        self.rules.retain(|r| r.exchange_id != ex);
        self.rules.push(MarketRule {
            market_id: market.into(),
            exchange_id: ex.into(),
            race_id: race.into(),
            kind: "race_party".into(),
            party: party.into(),
            verified,
            ..Default::default()
        });
        self.reindex();
    }

    /// Edit the existing rule of `ex`.
    pub fn rule_with(&mut self, ex: &str, edit: impl FnOnce(&mut MarketRule)) {
        if let Some(r) = self.rules.iter_mut().find(|r| r.exchange_id == ex) {
            edit(r);
        }
        self.reindex();
    }

    fn reindex(&mut self) {
        self.markets = MarketIndex::new(&MarketMap {
            header: None,
            markets: self.rules.clone(),
        });
    }

    /// Install a fresh book (observed 1 s ago) from `(raw pips, qty)` rungs.
    pub fn book(&mut self, ex: &str, bids: &[(u32, i64)], asks: &[(u32, i64)]) {
        self.book_at(ex, bids, asks, self.now_ns - NS_PER_S);
    }

    /// Install a book older than the freshness limit.
    pub fn stale_book(&mut self, ex: &str, bids: &[(u32, i64)], asks: &[(u32, i64)]) {
        self.book_at(ex, bids, asks, self.now_ns - self.book_max_age_ns - NS_PER_S);
    }

    fn book_at(&mut self, ex: &str, bids: &[(u32, i64)], asks: &[(u32, i64)], at: i64) {
        let lv = |v: &[(u32, i64)]| {
            v.iter()
                .map(|&(price, qty)| Level { price: p(price), qty })
                .collect()
        };
        self.books
            .apply_snapshot(L2Book::from_levels(ExchangeId::new(ex), lv(bids), lv(asks), at));
    }

    /// Set a fair quote observed 1 s ago.
    pub fn fair(&mut self, ex: &str, p_yes: f64, prev: Option<f64>) {
        self.fair_at(ex, p_yes, prev, self.now_ns - NS_PER_S);
    }

    /// Set a fair quote observed at `at` ns.
    pub fn fair_at(&mut self, ex: &str, p_yes: f64, prev: Option<f64>, at: i64) {
        self.fair.insert(
            ExchangeId::new(ex),
            FairQuote {
                p_yes,
                observed_at_ns: at,
                prev_p_yes: prev,
                source: "test".into(),
            },
        );
    }

    /// Set a signed position.
    pub fn position(&mut self, ex: &str, qty: i64) {
        self.positions.insert(ExchangeId::new(ex), qty);
    }

    /// Register a relationship.
    pub fn relationship(&mut self, rel: Relationship) {
        self.relationships.insert(rel.id.clone(), rel);
    }

    /// Append a target row (creating the file on first use).
    pub fn target(&mut self, ex: &str, target_qty: i64, max_entry_price: f64, locked: bool) {
        self.targets
            .get_or_insert_with(TargetFile::default)
            .positions
            .push(TargetPosition {
                exchange_id: ex.into(),
                target_qty,
                max_entry_price,
                locked,
                ..Default::default()
            });
    }

    /// Install a needle view with one posterior per `(race, p_dem, p_rep, n_batches)`.
    pub fn needle(&mut self, healthy: bool, races: &[(&str, f64, f64, u32)]) {
        self.needle = Some(NeedleView {
            healthy,
            epoch: 1,
            posteriors: races
                .iter()
                .map(|&(race, p_dem, p_rep, n_batches)| {
                    (
                        race.to_owned(),
                        RacePosterior {
                            race_id: race.into(),
                            p_dem,
                            p_rep,
                            n_batches,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        });
    }
}

/// A live intent with 10 shares remaining, created 5 s before [`NOW_S`].
pub fn intent(id: &str, strategy: &str, ex: &str, side: Side, action: Action, price: u32) -> IntentView {
    IntentView {
        intent_id: id.into(),
        strategy: strategy.into(),
        exchange_id: ExchangeId::new(ex),
        side,
        action,
        price: p(price),
        remaining: 10,
        created_at_ns: (NOW_S - 5) * NS_PER_S,
        needle_derived: false,
    }
}

/// A `violated` engine evaluation for a relationship.
pub fn violation(relationship_id: &str, kind: RelationshipType) -> ConstraintEval {
    ConstraintEval {
        relationship_id: relationship_id.into(),
        kind,
        violation_amount: 0.05,
        reason: String::new(),
        suggested_corrective_trades: Vec::new(),
        observed_prices: None,
        bound: None,
        lower_bound: None,
        upper_bound: None,
        current_value: None,
        evaluation_status: "violated".into(),
        diagnostic: None,
        tournament_id: None,
        direction: None,
        computed_at: None,
    }
}

/// Book rungs as `(raw pips, qty)`.
pub type Rungs = Vec<(u32, i64)>;

/// A random uncrossed book as `(bids, asks)`.
pub fn arb_book() -> impl Strategy<Value = (Rungs, Rungs)> {
    (
        2u32..=198,
        proptest::collection::vec((0u32..1000, 1i64..40), 0..4),
        proptest::collection::vec((0u32..1000, 1i64..40), 0..4),
    )
        .prop_map(|(split, bids, asks)| {
            let bids = bids
                .into_iter()
                .map(|(o, q)| ((1 + o % (split - 1)) * 50, q))
                .collect();
            let asks = asks
                .into_iter()
                .map(|(o, q)| ((split + o % (200 - split)) * 50, q))
                .collect();
            (bids, asks)
        })
}

/// A relationship node on `ex` with `role`.
pub fn node(ex: &str, role: NodeRole) -> RelationshipNode {
    RelationshipNode {
        exchange_id: ExchangeId::new(ex),
        market_id: None,
        market_title: None,
        outcome: None,
        current_price: None,
        role,
    }
}

/// An active relationship.
pub fn rel(id: &str, kind: RelationshipType, exhaustive: bool, nodes: Vec<RelationshipNode>) -> Relationship {
    Relationship {
        id: id.into(),
        kind,
        source: "engine".into(),
        status: "active".into(),
        label: None,
        reason: None,
        is_exhaustive: exhaustive,
        nodes,
        direction: None,
        constraint: None,
        version: 1,
    }
}
