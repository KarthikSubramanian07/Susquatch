//! Everything the engine knows about the outside world, refreshed from REST, realtime, files and
//! the bus. Pure data plus small helpers that turn it into strategy and risk inputs.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use susq_book::BookStore;
use susq_contracts::v1::{HealthItem, TradeSummary};
use susq_core::{ExchangeId, MarketIndex, TournamentId, side::yes_space};
use susq_oms::{Oms, PositionTracker, Tracked};
use susq_platform::{ConstraintEval, Relationship};
use susq_risk::LiveOrder;
use susq_strategies::{FairQuote, IntentView, NeedleView};

/// How many notable events and trades the status snapshot keeps.
const KEEP: usize = 50;

/// Mutable view of the world.
#[derive(Debug, Default)]
pub struct World {
    pub tid: TournamentId,
    pub slug: String,
    pub markets: MarketIndex,
    pub books: BookStore,
    pub positions: PositionTracker,
    /// Account value the leaderboard ranks on (`pnl.totalAccountValue`).
    pub equity: f64,
    /// Equity when this process started (for `pnl_total`).
    pub initial_equity: f64,
    pub cash: f64,
    pub relationships: HashMap<String, Relationship>,
    /// Current violations by relationship id, with when we learned about them (ns).
    pub violations: BTreeMap<String, (ConstraintEval, i64)>,
    pub fair: HashMap<ExchangeId, FairQuote>,
    pub needle: Option<NeedleView>,
    pub needle_errors_per_min: u32,
    pub health: BTreeMap<String, HealthItem>,
    pub events: VecDeque<String>,
    pub trades: VecDeque<TradeSummary>,
    pub action_needed: BTreeSet<String>,
    /// Glitch-trade notional committed so far (risk glitch cap).
    pub glitch_used: f64,
    pub rank: Option<i64>,
    pub field_size: u32,
}

impl World {
    /// Record a health item (`state` is `ok` | `degraded` | `down`).
    pub fn set_health(&mut self, name: &str, state: &str, detail: impl Into<String>, now_ns: i64) {
        let last_ok_ns = if state == "ok" {
            now_ns
        } else {
            self.health.get(name).map_or(0, |h| h.last_ok_ns)
        };
        self.health.insert(
            name.to_owned(),
            HealthItem {
                name: name.to_owned(),
                state: state.to_owned(),
                detail: detail.into(),
                last_ok_ns,
            },
        );
    }

    /// Put a health item produced elsewhere (targets, needle).
    pub fn put_health(&mut self, item: HealthItem) {
        self.health.insert(item.name.clone(), item);
    }

    /// Remember a notable event for the status snapshot and the daily summary.
    pub fn event(&mut self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!(event = %text);
        self.events.push_back(text);
        while self.events.len() > KEEP {
            self.events.pop_front();
        }
    }

    /// Remember a trade for the status snapshot.
    pub fn trade(&mut self, t: TradeSummary) {
        self.trades.push_back(t);
        while self.trades.len() > KEEP {
            self.trades.pop_front();
        }
    }

    /// Violations as a slice-friendly list.
    pub fn violation_list(&self) -> Vec<ConstraintEval> {
        self.violations.values().map(|(v, _)| v.clone()).collect()
    }

    /// YES marks: REST valuation where we hold a position, else the book mid.
    pub fn marks(&self) -> HashMap<ExchangeId, f64> {
        let mut out = HashMap::new();
        for ex in self.positions.map().keys() {
            if let Some(m) = self.positions.mark(ex) {
                out.insert(ex.clone(), m);
            }
        }
        for (ex, f) in &self.fair {
            if let Some(mid) = self.books.get(ex).and_then(|b| b.mid()) {
                out.entry(ex.clone()).or_insert(mid);
            } else {
                out.entry(ex.clone()).or_insert(f.p_yes);
            }
        }
        out
    }

    /// Exchanges whose books the strategies need this round.
    pub fn exchanges_of_interest(
        &self,
        targets: Option<&susq_contracts::v1::TargetFile>,
    ) -> BTreeSet<ExchangeId> {
        let mut out: BTreeSet<ExchangeId> = self.positions.map().keys().cloned().collect();
        for (v, _) in self.violations.values() {
            if let Some(rel) = self.relationships.get(&v.relationship_id) {
                out.extend(rel.nodes.iter().map(|n| n.exchange_id.clone()));
            }
            out.extend(
                v.suggested_corrective_trades
                    .iter()
                    .map(|t| t.exchange_id.clone()),
            );
        }
        if let Some(t) = targets {
            out.extend(t.positions.iter().map(|p| ExchangeId::new(p.exchange_id.clone())));
        }
        out.extend(self.fair.keys().cloned());
        if let Some(n) = self.needle.as_ref().filter(|n| n.healthy) {
            for race in n.posteriors.keys() {
                out.extend(self.markets.exchanges_for_race(race).iter().cloned());
            }
        }
        out
    }
}

fn live_terms(t: &Tracked) -> Option<(susq_core::Side, susq_core::Action, susq_core::Pips)> {
    t.state.is_live().then(|| t.order_terms()).flatten()
}

/// Our live intents as strategies see them.
pub fn intent_views(oms: &Oms) -> Vec<IntentView> {
    oms.live()
        .filter_map(|t| {
            let (side, action, price) = live_terms(t)?;
            Some(IntentView {
                intent_id: t.intent.intent_id.clone(),
                strategy: t.intent.strategy.clone(),
                exchange_id: t.exchange_id(),
                side,
                action,
                price,
                remaining: t.remaining,
                created_at_ns: t.intent.created_at_ns,
                needle_derived: t.meta.needle_derived,
            })
        })
        .collect()
}

/// Our live orders as risk sees them (everything not terminal counts as if it will fill).
pub fn live_orders(oms: &Oms) -> Vec<LiveOrder> {
    oms.live()
        .filter_map(|t| {
            let (side, action, price) = live_terms(t)?;
            let (book_side, yes_price) = yes_space(side, action, price);
            Some(LiveOrder {
                exchange_id: t.exchange_id(),
                book_side,
                yes_price,
                remaining: t.remaining,
            })
        })
        .collect()
}
