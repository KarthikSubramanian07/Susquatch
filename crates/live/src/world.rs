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

use crate::enr_health::EnrView;

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
    /// Latest assessment of the results pollers (`--election` only).
    pub enr: Option<EnrView>,
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

    /// Exchanges whose books the strategies need this round. Only fresh fair values pull their
    /// books in (stale ones can't be traded on anyway).
    pub fn exchanges_of_interest(
        &self,
        targets: Option<&susq_contracts::v1::TargetFile>,
        now_ns: i64,
        fair_max_age_ns: i64,
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
        out.extend(
            self.fair
                .iter()
                .filter(|(_, q)| now_ns - q.observed_at_ns <= fair_max_age_ns)
                .map(|(ex, _)| ex.clone()),
        );
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

/// SUSQies the election-night executor has deployed, for the risk night budget: every
/// needle-derived buy at its limit for the shares bought or still resting, less the proceeds of
/// needle-derived sells (rolls). An intent recovered from a previous run carries no fill count
/// and no `needle_derived` flag, so it is recognised by its strategy name and a closed one is
/// counted as fully bought (a sell as returning nothing): after a restart the budget can only
/// look more used than it is, never less.
pub fn needle_budget_used(oms: &Oms) -> f64 {
    let used: f64 = oms
        .all()
        .filter(|t| t.meta.needle_derived || t.intent.strategy == susq_strategies::en_executor::NAME)
        .filter_map(|t| {
            let (_, action, price) = t.order_terms()?;
            let per_share = price.prob();
            let shares = if t.meta.restored && t.state.is_terminal() {
                match action {
                    susq_core::Action::Buy => t.intent.quantity,
                    susq_core::Action::Sell => 0,
                }
            } else {
                match action {
                    susq_core::Action::Buy => t.filled + t.remaining.max(0),
                    susq_core::Action::Sell => t.filled,
                }
            };
            let signed = match action {
                susq_core::Action::Buy => 1.0,
                susq_core::Action::Sell => -1.0,
            };
            Some(signed * shares.max(0) as f64 * per_share)
        })
        .sum();
    used.max(0.0)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use susq_core::{Action, Calendar, ExchangeId, OrderBundle, OrderSpec, Pips, Qty, Side, TournamentId};
    use susq_oms::{Oms, OmsConfig};
    use susq_platform::OrderAck;

    use super::needle_budget_used;

    fn cfg() -> OmsConfig {
        OmsConfig {
            namespace: uuid::Uuid::parse_str("6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99").expect("ns"),
            sent_timeout_ns: 20_000_000_000,
            max_resend_age_ns: 30_000_000_000,
        }
    }

    fn calendar() -> Calendar {
        Calendar {
            open_utc: "2026-10-01T16:00:00Z".parse().expect("ts"),
            lock_utc: "2026-11-04T17:00:00Z".parse().expect("ts"),
            last_order_utc: "2026-11-04T16:55:00Z".parse().expect("ts"),
        }
    }

    fn t0() -> DateTime<Utc> {
        "2026-11-04T01:00:00Z".parse().expect("ts")
    }

    fn spec(id: &str, strategy: &str, action: Action, qty: i64, price: u32, needle: bool) -> OrderSpec {
        OrderSpec {
            intent_id: id.into(),
            strategy: strategy.into(),
            exchange_id: ExchangeId::new("1"),
            market_id: None,
            side: Side::Yes,
            action,
            quantity: Qty(qty),
            price: Pips::new(price).expect("valid"),
            ttl: Duration::from_secs(20),
            needle_derived: needle,
            fair_yes: Some(0.9),
        }
    }

    fn ack(open: bool, traded: f64) -> OrderAck {
        OrderAck {
            order_id: Some(7),
            exchange_id: ExchangeId::new("1"),
            open,
            remaining_quantity: None,
            action: None,
            side: None,
            price: None,
            quantity: None,
            terminal_reason_code: None,
            quantity_traded: traded,
            total_cost: 0.0,
            fill_price: None,
            all: None,
        }
    }

    fn send(oms: &mut Oms, s: OrderSpec, ack_: &OrderAck) {
        let id = s.intent_id.clone();
        oms.prepare(
            &OrderBundle::single(s),
            t0(),
            &calendar(),
            &TournamentId::new("t"),
        )
        .expect("prepare");
        assert!(oms.mark_sent(std::slice::from_ref(&id), 0));
        oms.apply_ack(&id, ack_, 0);
    }

    /// Gap 10: the night budget counts what the needle bought (at cost) and still has resting,
    /// less roll proceeds; other strategies never count; a restart can only make it look fuller.
    #[test]
    fn needle_budget_counts_buys_resting_and_rolls() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("intents.jsonl");
        {
            let mut oms = Oms::open(cfg(), &path, 0).expect("open");
            assert_eq!(needle_budget_used(&oms), 0.0);
            // 100 bought at 0.60, the order closed: 60.
            send(
                &mut oms,
                spec("en:1:yes:1", "en", Action::Buy, 100, 6000, true),
                &ack(false, 100.0),
            );
            assert!((needle_budget_used(&oms) - 60.0).abs() < 1e-9);
            // 50 resting at 0.70, 0 filled yet: +35.
            send(
                &mut oms,
                spec("en:1:yes:2", "en", Action::Buy, 50, 7000, true),
                &ack(true, 0.0),
            );
            assert!((needle_budget_used(&oms) - 95.0).abs() < 1e-9);
            // A roll sold 100 at 0.90: frees 90.
            send(
                &mut oms,
                spec("en:1:roll:3", "en", Action::Sell, 100, 9000, true),
                &ack(false, 100.0),
            );
            assert!((needle_budget_used(&oms) - 5.0).abs() < 1e-9);
            // The sniper is not the needle's budget.
            send(
                &mut oms,
                spec("sniper:1:1", "sniper", Action::Buy, 1_000, 9500, false),
                &ack(false, 1_000.0),
            );
            assert!((needle_budget_used(&oms) - 5.0).abs() < 1e-9);
        }
        // Restart: fills are not in the intent log, so closed needle buys count in full, sells as
        // nothing, and the resting buy (restored, unresolved) still counts at its limit.
        let reopened = Oms::open(cfg(), &path, 1).expect("reopen");
        assert!((needle_budget_used(&reopened) - (60.0 + 35.0)).abs() < 1e-9);
    }
}
