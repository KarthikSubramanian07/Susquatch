//! The order manager: write-ahead intents, acks, reconcile, restart recovery.
//!
//! Invariants (tested, including a proptest over random interleavings):
//! 1. An `intent_id` is prepared at most once (refused for 48 h after it closes), so a re-decided
//!    intent can never mint a second key.
//! 2. Every leg is on disk (`PENDING`, then `SENT`) before its request is sent.
//! 3. Retries and resends always rebuild the byte-identical payload from the stored intent, so they
//!    carry the same idempotency key and the engine replays instead of placing again.
//! 4. After a restart nothing is resent: in-flight intents are resolved from REST (open orders and
//!    positions), and untracked resting orders ("orphans") are cancelled.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use susq_contracts::v1::OrderIntent;
use susq_core::{
    Action, Calendar, ExchangeId, MarketId, OrderBundle, OrderId, Pips, Qty, Side, TournamentId,
    clock::{from_ns, to_ns},
    side::yes_space,
};
use susq_platform::{
    MAX_LEGS, MultiLegRequest, Order, OrderAck, OrderRequest, PlaceOrderRequest, models::CancelOutcome,
};
use uuid::Uuid;

use crate::{
    idempotency::{PayloadParts, bundle_key, canonical_payload, idempotency_key},
    state::OrderState,
    store::IntentStore,
};

/// Tuning for reconcile.
#[derive(Debug, Clone)]
pub struct OmsConfig {
    /// uuid5 namespace (`oms.idempotency_namespace`).
    pub namespace: Uuid,
    /// A `SENT` intent with no response after this long becomes `UNKNOWN`.
    pub sent_timeout_ns: i64,
    /// `UNKNOWN` intents younger than this are resent with the same key; older ones are left to
    /// orphan handling (their decision is stale).
    pub max_resend_age_ns: i64,
}

/// Why a bundle was not prepared. Nothing was written or sent.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PrepareError {
    #[error("intent {0} was already prepared")]
    Duplicate(String),
    #[error("time gate: no valid expiration before the last-order cutoff")]
    TimeGate,
    #[error("empty bundle")]
    Empty,
    #[error("atomic bundle of {0} legs exceeds {MAX_LEGS}")]
    TooManyLegs(usize),
    #[error("missing tournament id")]
    NoTournament,
    #[error("writing intent log: {0}")]
    Io(String),
}

/// Strategy metadata that is not part of the persisted contract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegMeta {
    pub needle_derived: bool,
    pub market_id: Option<MarketId>,
    /// Multi-leg bundle this leg belongs to.
    pub bundle_id: Option<String>,
    pub fair_yes: Option<f64>,
    /// Loaded from disk after a restart: never resent, only reconciled.
    pub restored: bool,
}

/// One intent and what we know about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tracked {
    pub intent: OrderIntent,
    pub state: OrderState,
    pub meta: LegMeta,
    /// Shares filled so far (from acks and reconcile).
    pub filled: i64,
    /// Executable remainder while resting.
    pub remaining: i64,
    /// A cancel was requested before an order id was known: cancel as soon as it is acked.
    pub cancel_requested: bool,
}

impl Tracked {
    /// Platform order id once acked.
    pub fn order_id(&self) -> Option<OrderId> {
        (self.intent.order_id != 0).then_some(OrderId(self.intent.order_id))
    }

    /// Exchange id.
    pub fn exchange_id(&self) -> ExchangeId {
        ExchangeId::new(self.intent.exchange_id.clone())
    }

    /// Side, action and price as core types (None only for corrupt records).
    pub fn order_terms(&self) -> Option<(Side, Action, Pips)> {
        let side = Side::try_from(susq_contracts::v1::Side::try_from(self.intent.side).ok()?).ok()?;
        let action = Action::try_from(susq_contracts::v1::Action::try_from(self.intent.action).ok()?).ok()?;
        Some((side, action, Pips::new(self.intent.price_pips).ok()?))
    }
}

/// What to send for a prepared bundle.
#[derive(Debug, Clone, PartialEq)]
pub enum Prepared {
    Single {
        intent_id: String,
        request: PlaceOrderRequest,
    },
    MultiLeg {
        bundle_id: String,
        intent_ids: Vec<String>,
        request: MultiLegRequest,
    },
}

impl Prepared {
    /// Intent ids covered by this request.
    pub fn intent_ids(&self) -> Vec<String> {
        match self {
            Prepared::Single { intent_id, .. } => vec![intent_id.clone()],
            Prepared::MultiLeg { intent_ids, .. } => intent_ids.clone(),
        }
    }

    /// The idempotency key of the request.
    pub fn key(&self) -> &str {
        match self {
            Prepared::Single { request, .. } => &request.idempotency_key,
            Prepared::MultiLeg { request, .. } => &request.idempotency_key,
        }
    }
}

/// Position change implied by an ack (for the optimistic position tracker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillDelta {
    pub exchange_id: ExchangeId,
    pub yes_delta: i64,
}

/// Outcome of a reconcile pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileReport {
    /// Unknown intents to resend with their original key (same payload).
    pub resend: Vec<Prepared>,
    /// Untracked resting orders seen twice in a row with nothing in flight on their exchange.
    pub cancel_orphans: Vec<OrderId>,
    /// Acked orders that must be cancelled because a cancel was requested before the ack.
    pub cancel_requested: Vec<OrderId>,
    /// Intents that closed during this pass.
    pub closed: Vec<String>,
}

/// The order manager.
#[derive(Debug)]
pub struct Oms {
    cfg: OmsConfig,
    store: IntentStore,
    orders: BTreeMap<String, Tracked>,
    orphan_candidates: HashSet<OrderId>,
    illegal_transitions: u64,
}

fn now_s(t: DateTime<Utc>) -> i64 {
    t.timestamp()
}

impl Oms {
    /// Open the intent log and recover. Every non-terminal intent from a previous run is marked
    /// `restored`; unsent/in-flight ones become `UNKNOWN` and are resolved by reconcile, never resent.
    pub fn open(cfg: OmsConfig, path: &std::path::Path, now_ns: i64) -> std::io::Result<Self> {
        let (store, loaded) = IntentStore::open(path, now_ns)?;
        let mut oms = Self {
            cfg,
            store,
            orders: BTreeMap::new(),
            orphan_candidates: HashSet::new(),
            illegal_transitions: 0,
        };
        for (id, intent) in loaded {
            let mut state = OrderState::from_proto(intent.state, &intent.last_error_code);
            if matches!(state, OrderState::Pending | OrderState::Sent) {
                state = OrderState::Unknown;
            }
            let bundle_id = id.split_once('#').map(|(b, _)| b.to_owned());
            let meta = LegMeta {
                restored: true,
                bundle_id,
                ..LegMeta::default()
            };
            let remaining = if state.is_terminal() { 0 } else { intent.quantity };
            oms.orders.insert(
                id,
                Tracked {
                    intent,
                    state,
                    meta,
                    filled: 0,
                    remaining,
                    cancel_requested: false,
                },
            );
        }
        Ok(oms)
    }

    /// Intent log corruption seen at open (for health).
    pub fn corrupt_lines(&self) -> usize {
        self.store.corrupt_lines
    }

    /// Transitions refused as illegal since start (should stay 0).
    pub fn illegal_transitions(&self) -> u64 {
        self.illegal_transitions
    }

    /// Look up an intent.
    pub fn get(&self, intent_id: &str) -> Option<&Tracked> {
        self.orders.get(intent_id)
    }

    /// Every intent that may still be (or become) resting.
    pub fn live(&self) -> impl Iterator<Item = &Tracked> {
        self.orders.values().filter(|t| t.state.is_live())
    }

    /// Every tracked intent (live and recently closed).
    pub fn all(&self) -> impl Iterator<Item = &Tracked> {
        self.orders.values()
    }

    /// True if an intent id (or a bundle id) is already known.
    pub fn knows(&self, id: &str) -> bool {
        self.orders.contains_key(id) || self.orders.contains_key(&format!("{id}#0"))
    }

    /// Validate, compute expirations and keys, and durably write every leg as `PENDING`.
    pub fn prepare(
        &mut self,
        bundle: &OrderBundle,
        now: DateTime<Utc>,
        calendar: &Calendar,
        tid: &TournamentId,
    ) -> Result<Vec<Prepared>, PrepareError> {
        if bundle.legs.is_empty() {
            return Err(PrepareError::Empty);
        }
        if tid.as_str().is_empty() {
            return Err(PrepareError::NoTournament);
        }
        let atomic = bundle.atomic && bundle.legs.len() > 1;
        if atomic && bundle.legs.len() > MAX_LEGS {
            return Err(PrepareError::TooManyLegs(bundle.legs.len()));
        }
        let bundle_id = atomic.then(|| bundle.legs[0].intent_id.clone());
        let ids: Vec<String> = match &bundle_id {
            Some(b) => (0..bundle.legs.len()).map(|i| format!("{b}#{i}")).collect(),
            None => bundle.legs.iter().map(|l| l.intent_id.clone()).collect(),
        };
        let mut seen = HashSet::new();
        for id in ids.iter().chain(bundle_id.iter()) {
            if self.knows(id) || !seen.insert(id.clone()) {
                return Err(PrepareError::Duplicate(id.clone()));
            }
        }

        let now_ns = to_ns(now);
        let mut legs = Vec::with_capacity(bundle.legs.len());
        for (spec, id) in bundle.legs.iter().zip(&ids) {
            let expiration = calendar
                .order_expiration(now, spec.ttl)
                .ok_or(PrepareError::TimeGate)?;
            let parts = PayloadParts {
                exchange_id: &spec.exchange_id,
                side: spec.side,
                action: spec.action,
                quantity: spec.quantity,
                price: spec.price,
                expiration_unix_s: now_s(expiration),
                tournament_id: tid,
            };
            let payload = canonical_payload(&parts);
            let intent = OrderIntent {
                intent_id: id.clone(),
                idempotency_key: idempotency_key(&self.cfg.namespace, id, &payload).to_string(),
                strategy: spec.strategy.clone(),
                exchange_id: spec.exchange_id.to_string(),
                side: susq_contracts::v1::Side::from(spec.side) as i32,
                action: susq_contracts::v1::Action::from(spec.action) as i32,
                quantity: spec.quantity.get(),
                price_pips: spec.price.get(),
                expiration_ns: to_ns(expiration),
                tournament_id: tid.to_string(),
                state: OrderState::Pending.to_proto() as i32,
                created_at_ns: now_ns,
                updated_at_ns: now_ns,
                ..Default::default()
            };
            let meta = LegMeta {
                needle_derived: spec.needle_derived,
                market_id: spec.market_id.clone(),
                bundle_id: bundle_id.clone(),
                fair_yes: spec.fair_yes,
                restored: false,
            };
            legs.push((intent, meta, payload));
        }
        if let Some(b) = &bundle_id {
            let payloads: Vec<String> = legs.iter().map(|(_, _, p)| p.clone()).collect();
            let key = bundle_key(&self.cfg.namespace, b, &payloads).to_string();
            for (intent, _, _) in &mut legs {
                intent.idempotency_key.clone_from(&key);
            }
        }
        for (intent, meta, _) in legs {
            self.store
                .append(&intent)
                .map_err(|e| PrepareError::Io(e.to_string()))?;
            let remaining = intent.quantity;
            self.orders.insert(
                intent.intent_id.clone(),
                Tracked {
                    intent,
                    state: OrderState::Pending,
                    meta,
                    filled: 0,
                    remaining,
                    cancel_requested: false,
                },
            );
        }
        Ok(match bundle_id {
            Some(b) => vec![self.multi_leg_request(&b).ok_or(PrepareError::Empty)?],
            None => ids.iter().filter_map(|id| self.single_request(id)).collect(),
        })
    }

    fn order_request(t: &Tracked) -> Option<OrderRequest> {
        let (side, action, price) = t.order_terms()?;
        Some(OrderRequest {
            exchange_id: t.exchange_id(),
            side,
            action,
            quantity: Qty::order(t.intent.quantity)?.get(),
            price,
            expiration_date: from_ns(t.intent.expiration_ns),
            tournament_id: TournamentId::new(t.intent.tournament_id.clone()),
        })
    }

    /// Rebuild the exact `POST /orders` request of a single intent (same payload, same key).
    pub fn single_request(&self, intent_id: &str) -> Option<Prepared> {
        let t = self.orders.get(intent_id)?;
        Some(Prepared::Single {
            intent_id: intent_id.to_owned(),
            request: PlaceOrderRequest {
                order: Self::order_request(t)?,
                idempotency_key: t.intent.idempotency_key.clone(),
            },
        })
    }

    /// Rebuild the exact multi-leg request of a bundle.
    pub fn multi_leg_request(&self, bundle_id: &str) -> Option<Prepared> {
        let mut intent_ids = Vec::new();
        let mut legs = Vec::new();
        let mut key = None;
        for i in 0..MAX_LEGS {
            let id = format!("{bundle_id}#{i}");
            let Some(t) = self.orders.get(&id) else { break };
            legs.push(Self::order_request(t)?);
            key.get_or_insert_with(|| t.intent.idempotency_key.clone());
            intent_ids.push(id);
        }
        Some(Prepared::MultiLeg {
            bundle_id: bundle_id.to_owned(),
            intent_ids,
            request: MultiLegRequest {
                legs,
                idempotency_key: key?,
                relationship_constraint: None,
            },
        })
    }

    fn transition(&mut self, intent_id: &str, to: OrderState, now_ns: i64, code: Option<&str>) -> bool {
        let Some(t) = self.orders.get_mut(intent_id) else {
            return false;
        };
        if t.state == to && to == OrderState::PartiallyFilled {
            // Re-entrant: only the fill counters changed.
        } else if !t.state.can_transition(to) {
            self.illegal_transitions += 1;
            tracing::warn!(intent_id, from = ?t.state, to = ?to, "refused illegal order transition");
            return false;
        }
        t.state = to;
        t.intent.state = to.to_proto() as i32;
        t.intent.updated_at_ns = now_ns;
        if let Some(c) = code.or(to.done_reason()) {
            t.intent.last_error_code = c.to_owned();
        }
        if to.is_terminal() {
            t.remaining = 0;
        }
        let snapshot = t.intent.clone();
        if let Err(e) = self.store.append(&snapshot) {
            // The in-memory state is still correct; a lost line only means restart recovery is
            // more conservative (it will treat the intent as unresolved).
            tracing::error!(intent_id, error = %e, "failed to persist intent transition");
        }
        true
    }

    /// Durably mark intents as on the wire; call immediately before sending. Returns `false` (and
    /// changes nothing) unless every leg may legally be sent now — e.g. an intent reconcile already
    /// abandoned or closed must never reach the wire. Increments attempts.
    #[must_use = "only send when mark_sent returns true"]
    pub fn mark_sent(&mut self, intent_ids: &[String], now_ns: i64) -> bool {
        let sendable = !intent_ids.is_empty()
            && intent_ids.iter().all(|id| {
                self.orders
                    .get(id)
                    .is_some_and(|t| t.state.can_transition(OrderState::Sent))
            });
        if !sendable {
            return false;
        }
        for id in intent_ids {
            if let Some(t) = self.orders.get_mut(id) {
                t.intent.attempts += 1;
            }
            self.transition(id, OrderState::Sent, now_ns, None);
        }
        true
    }

    /// Apply a success response. Idempotent for replays: fill deltas are only the new fills.
    pub fn apply_ack(&mut self, intent_id: &str, ack: &OrderAck, now_ns: i64) -> Option<FillDelta> {
        let t = self.orders.get_mut(intent_id)?;
        if let Some(id) = ack.order_id {
            t.intent.order_id = id;
        }
        let traded = ack.quantity_traded.max(0.0).round() as i64;
        let new_fill = (traded - t.filled).max(0);
        t.filled = t.filled.max(traded);
        t.remaining = ack
            .remaining_quantity
            .map(|r| r.max(0.0).round() as i64)
            .unwrap_or((t.intent.quantity - t.filled).max(0));
        let to = if !ack.open {
            if ack.terminal_reason_code.is_some() || t.filled < t.intent.quantity {
                OrderState::Cancelled
            } else {
                OrderState::Filled
            }
        } else if t.filled > 0 {
            OrderState::PartiallyFilled
        } else {
            OrderState::Acked
        };
        let (side, action, price) = t.order_terms()?;
        let side = ack.side.unwrap_or(side);
        let action = ack.action.unwrap_or(action);
        let exchange_id = t.exchange_id();
        let code = ack.terminal_reason_code.clone();
        if t.state != to {
            self.transition(intent_id, to, now_ns, code.as_deref());
        }
        (new_fill > 0).then(|| FillDelta {
            exchange_id,
            yes_delta: yes_space(side, action, price).0.yes_delta_sign() * new_fill,
        })
    }

    /// Terminal rejection (4xx other than the retryable ones).
    pub fn apply_reject(&mut self, intent_id: &str, code: &str, now_ns: i64) {
        self.transition(intent_id, OrderState::Rejected, now_ns, Some(code));
    }

    /// Outcome unknown after the retry budget (502, timeout, network).
    pub fn apply_unknown(&mut self, intent_id: &str, code: &str, now_ns: i64) {
        self.transition(intent_id, OrderState::Unknown, now_ns, Some(code));
    }

    /// A cancel was confirmed (or the order was already closed).
    pub fn apply_cancel(&mut self, intent_id: &str, outcome: CancelOutcome, now_ns: i64) {
        let reason = match outcome {
            CancelOutcome::Cancelled => "CANCELLED",
            CancelOutcome::AlreadyClosed => "CLOSED",
        };
        self.transition(intent_id, OrderState::Cancelled, now_ns, Some(reason));
    }

    /// Choose what to cancel: acked orders are returned as `(intent_id, order_id)`; intents still in
    /// flight are flagged so they are cancelled as soon as an order id is known.
    pub fn request_cancel(&mut self, pred: impl Fn(&Tracked) -> bool) -> Vec<(String, OrderId)> {
        let mut out = Vec::new();
        for (id, t) in self
            .orders
            .iter_mut()
            .filter(|(_, t)| t.state.is_live() && pred(t))
        {
            match t.order_id() {
                Some(oid) => out.push((id.clone(), oid)),
                None => t.cancel_requested = true,
            }
        }
        out
    }

    /// Mark a never-sent (`PENDING`) intent as abandoned, e.g. when a gate refused it after prepare.
    pub fn abandon(&mut self, intent_id: &str, reason: &str, now_ns: i64) {
        if self
            .orders
            .get(intent_id)
            .is_some_and(|t| t.state == OrderState::Pending)
        {
            self.transition(intent_id, OrderState::Cancelled, now_ns, Some(reason));
        }
    }

    /// Repair local state from the venue's open orders.
    pub fn reconcile(&mut self, open: &[Order], now_ns: i64) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let open_by_id: HashMap<i64, &Order> = open.iter().map(|o| (o.id, o)).collect();
        let tracked_ids: HashSet<i64> = self
            .orders
            .values()
            .map(|t| t.intent.order_id)
            .filter(|&i| i != 0)
            .collect();

        let ids: Vec<String> = self
            .orders
            .iter()
            .filter(|(_, t)| t.state.is_live())
            .map(|(k, _)| k.clone())
            .collect();
        let mut resend_bundles = HashSet::new();
        for id in ids {
            self.reconcile_one(&id, &open_by_id, now_ns, &mut report, &mut resend_bundles);
        }
        self.find_orphans(open, &tracked_ids, &mut report);
        report
    }

    fn reconcile_one(
        &mut self,
        id: &str,
        open_by_id: &HashMap<i64, &Order>,
        now_ns: i64,
        report: &mut ReconcileReport,
        resend_bundles: &mut HashSet<String>,
    ) {
        let Some(t) = self.orders.get(id) else { return };
        let (state, order_id, expiration_ns, created_ns, updated_ns) = (
            t.state,
            t.intent.order_id,
            t.intent.expiration_ns,
            t.intent.created_at_ns,
            t.intent.updated_at_ns,
        );
        let expired = now_ns >= expiration_ns;
        match state {
            OrderState::Acked | OrderState::PartiallyFilled => match open_by_id.get(&order_id) {
                Some(o) => {
                    let remaining = o.quantity.max(0.0).round() as i64;
                    let qty = t.intent.quantity;
                    let cancel_requested = t.cancel_requested;
                    if let Some(t) = self.orders.get_mut(id) {
                        t.remaining = remaining;
                        t.filled = t.filled.max(qty - remaining);
                    }
                    if remaining < qty && state == OrderState::Acked {
                        self.transition(id, OrderState::PartiallyFilled, now_ns, None);
                    }
                    if cancel_requested {
                        report.cancel_requested.push(OrderId(order_id));
                    }
                }
                None => {
                    let to = if expired {
                        OrderState::Expired
                    } else {
                        OrderState::Filled
                    };
                    let code = if expired { "EXPIRED" } else { "CLOSED" };
                    if self.transition(id, to, now_ns, Some(code)) {
                        report.closed.push(id.to_owned());
                    }
                }
            },
            OrderState::Sent if now_ns - updated_ns > self.cfg.sent_timeout_ns => {
                self.transition(id, OrderState::Unknown, now_ns, Some("NO_RESPONSE"));
            }
            OrderState::Unknown if expired => {
                if self.transition(id, OrderState::Expired, now_ns, Some("EXPIRED")) {
                    report.closed.push(id.to_owned());
                }
            }
            OrderState::Unknown if !t.meta.restored && now_ns - created_ns <= self.cfg.max_resend_age_ns => {
                let plan = match t.meta.bundle_id.clone() {
                    Some(b) if resend_bundles.insert(b.clone()) => self.multi_leg_request(&b),
                    Some(_) => None,
                    None => self.single_request(id),
                };
                report.resend.extend(plan);
            }
            OrderState::Pending if now_ns - created_ns > self.cfg.sent_timeout_ns => {
                self.transition(id, OrderState::Cancelled, now_ns, Some("ABANDONED"));
            }
            _ => {}
        }
    }

    fn find_orphans(&mut self, open: &[Order], tracked_ids: &HashSet<i64>, report: &mut ReconcileReport) {
        let in_flight: HashSet<String> = self
            .orders
            .values()
            .filter(|t| matches!(t.state, OrderState::Pending | OrderState::Sent))
            .map(|t| t.intent.exchange_id.clone())
            .collect();
        let mut candidates = HashSet::new();
        for o in open.iter().filter(|o| o.open && !tracked_ids.contains(&o.id)) {
            if in_flight.contains(o.exchange_id.as_str()) {
                continue;
            }
            let id = OrderId(o.id);
            if self.orphan_candidates.contains(&id) {
                report.cancel_orphans.push(id);
            }
            candidates.insert(id);
        }
        self.orphan_candidates = candidates;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use susq_core::OrderSpec;

    const NS: &str = "6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99";

    pub(crate) fn cfg() -> OmsConfig {
        OmsConfig {
            namespace: Uuid::parse_str(NS).expect("ns"),
            sent_timeout_ns: 20_000_000_000,
            max_resend_age_ns: 30_000_000_000,
        }
    }

    pub(crate) fn calendar() -> Calendar {
        Calendar {
            open_utc: "2026-10-01T16:00:00Z".parse().expect("ts"),
            lock_utc: "2026-11-04T17:00:00Z".parse().expect("ts"),
            last_order_utc: "2026-11-04T16:55:00Z".parse().expect("ts"),
        }
    }

    pub(crate) fn spec(id: &str, ex: &str, side: Side, qty: i64, price: u32) -> OrderSpec {
        OrderSpec {
            intent_id: id.into(),
            strategy: "racer".into(),
            exchange_id: ExchangeId::new(ex),
            market_id: None,
            side,
            action: Action::Buy,
            quantity: Qty(qty),
            price: Pips::new(price).expect("valid"),
            ttl: Duration::from_secs(30),
            needle_derived: false,
            fair_yes: None,
        }
    }

    fn t0() -> DateTime<Utc> {
        "2026-10-08T16:00:00Z".parse().expect("ts")
    }

    fn open_oms(dir: &std::path::Path) -> Oms {
        Oms::open(cfg(), &dir.join("intents.jsonl"), to_ns(t0())).expect("open")
    }

    fn ack(id: i64, ex: &str, open: bool, traded: f64) -> OrderAck {
        OrderAck {
            order_id: Some(id),
            exchange_id: ExchangeId::new(ex),
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

    #[test]
    fn prepare_uses_the_cross_language_key() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut oms = open_oms(dir.path());
        let now: DateTime<Utc> = "2026-10-08T16:00:00Z".parse().expect("ts"); // 1791475200
        let bundle = OrderBundle::single(spec("racer:rel-9:1791475200", "381", Side::No, 37, 1450));
        let prepared = oms
            .prepare(&bundle, now, &calendar(), &TournamentId::new("t-uuid"))
            .expect("prepare");
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].key(), "c5fe4b7e-7efc-55fb-82b5-1bd6356d699c");
        let Prepared::Single { request, .. } = &prepared[0] else {
            panic!("single")
        };
        assert_eq!(request.order.expiration_date.timestamp(), 1_791_475_230);
    }

    #[test]
    fn duplicate_intent_ids_are_refused_even_after_restart() {
        let dir = tempfile::tempdir().expect("tmp");
        let tid = TournamentId::new("t");
        let b = OrderBundle::single(spec("sniper:1:100", "1", Side::Yes, 5, 5000));
        {
            let mut oms = open_oms(dir.path());
            oms.prepare(&b, t0(), &calendar(), &tid).expect("first");
            let later = t0() + chrono::Duration::seconds(3);
            assert_eq!(
                oms.prepare(&b, later, &calendar(), &tid),
                Err(PrepareError::Duplicate("sniper:1:100".into()))
            );
        }
        let mut reopened = open_oms(dir.path());
        assert!(matches!(
            reopened.prepare(&b, t0(), &calendar(), &tid),
            Err(PrepareError::Duplicate(_))
        ));
    }

    #[test]
    fn time_gate_refuses_after_last_order() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut oms = open_oms(dir.path());
        let late: DateTime<Utc> = "2026-11-04T16:55:00Z".parse().expect("ts");
        let b = OrderBundle::single(spec("x:1:1", "1", Side::Yes, 1, 5000));
        assert_eq!(
            oms.prepare(&b, late, &calendar(), &TournamentId::new("t")),
            Err(PrepareError::TimeGate)
        );
        assert!(!oms.knows("x:1:1"), "a refused bundle writes nothing");
    }

    #[test]
    fn atomic_bundles_share_one_key_and_rebuild_identically() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut oms = open_oms(dir.path());
        let bundle = OrderBundle {
            legs: vec![
                spec("racer:r:1", "1", Side::No, 10, 4000),
                spec("racer:r:1", "2", Side::No, 10, 5000),
            ],
            atomic: true,
        };
        let p = oms
            .prepare(&bundle, t0(), &calendar(), &TournamentId::new("t"))
            .expect("prepare");
        let Prepared::MultiLeg {
            intent_ids, request, ..
        } = &p[0]
        else {
            panic!("multi")
        };
        assert_eq!(
            intent_ids,
            &vec!["racer:r:1#0".to_owned(), "racer:r:1#1".to_owned()]
        );
        assert_eq!(request.legs.len(), 2);
        assert_eq!(oms.multi_leg_request("racer:r:1").as_ref(), Some(&p[0]));
        assert!(oms.knows("racer:r:1"));
    }

    #[test]
    fn ack_replays_do_not_double_count_fills() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut oms = open_oms(dir.path());
        let b = OrderBundle::single(spec("a:1:1", "7", Side::No, 10, 4000));
        oms.prepare(&b, t0(), &calendar(), &TournamentId::new("t"))
            .expect("prepare");
        assert!(oms.mark_sent(&["a:1:1".into()], 1));
        let d = oms.apply_ack("a:1:1", &ack(5, "7", true, 4.0), 2).expect("fill");
        assert_eq!(d.yes_delta, -4, "buying NO is -YES");
        assert!(
            oms.apply_ack("a:1:1", &ack(5, "7", true, 4.0), 3).is_none(),
            "replay adds nothing"
        );
        assert_eq!(
            oms.get("a:1:1").map(|t| t.state),
            Some(OrderState::PartiallyFilled)
        );
        let d = oms
            .apply_ack("a:1:1", &ack(5, "7", false, 10.0), 4)
            .expect("rest filled");
        assert_eq!(d.yes_delta, -6);
        assert_eq!(oms.get("a:1:1").map(|t| t.state), Some(OrderState::Filled));
    }

    fn open_order(id: i64, ex: &str, qty: f64) -> Order {
        Order {
            id,
            exchange_id: ExchangeId::new(ex),
            side: Side::Yes,
            action: Action::Buy,
            quantity: qty,
            price_limit: Some(0.5),
            open: true,
            created_at: String::new(),
            expiration_date: None,
        }
    }

    #[test]
    fn reconcile_closes_expires_and_resends_unknowns_with_the_same_key() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut oms = open_oms(dir.path());
        let tid = TournamentId::new("t");
        let now = to_ns(t0());
        for (id, ex) in [("a:1:1", "1"), ("b:1:1", "2"), ("c:1:1", "3")] {
            oms.prepare(
                &OrderBundle::single(spec(id, ex, Side::Yes, 10, 5000)),
                t0(),
                &calendar(),
                &tid,
            )
            .expect("prepare");
            assert!(oms.mark_sent(&[id.to_owned()], now));
        }
        oms.apply_ack("a:1:1", &ack(1, "1", true, 0.0), now);
        oms.apply_ack("b:1:1", &ack(2, "2", true, 0.0), now);
        oms.apply_unknown("c:1:1", "ORDER_STATUS_UNKNOWN", now);
        let key_c = oms.get("c:1:1").map(|t| t.intent.idempotency_key.clone());

        let r = oms.reconcile(&[open_order(1, "1", 6.0)], now + 1);
        assert_eq!(
            oms.get("a:1:1").map(|t| (t.state, t.remaining)),
            Some((OrderState::PartiallyFilled, 6))
        );
        assert_eq!(oms.get("b:1:1").map(|t| t.state), Some(OrderState::Filled));
        assert_eq!(r.closed, vec!["b:1:1".to_owned()]);
        assert_eq!(r.resend.len(), 1);
        assert_eq!(Some(r.resend[0].key().to_owned()), key_c);

        let after_expiry = now + 31_000_000_000;
        let r = oms.reconcile(&[], after_expiry);
        assert!(r.resend.is_empty());
        assert_eq!(oms.get("c:1:1").map(|t| t.state), Some(OrderState::Expired));
        assert_eq!(oms.get("a:1:1").map(|t| t.state), Some(OrderState::Expired));
    }

    #[test]
    fn orphans_are_cancelled_only_when_seen_twice_and_nothing_in_flight() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut oms = open_oms(dir.path());
        let orphan = open_order(99, "5", 3.0);
        assert!(
            oms.reconcile(std::slice::from_ref(&orphan), 1)
                .cancel_orphans
                .is_empty()
        );
        assert_eq!(
            oms.reconcile(std::slice::from_ref(&orphan), 2).cancel_orphans,
            vec![OrderId(99)]
        );
        // With an intent in flight on the same exchange the order may be ours: leave it.
        oms.prepare(
            &OrderBundle::single(spec("d:1:1", "5", Side::Yes, 1, 5000)),
            t0(),
            &calendar(),
            &TournamentId::new("t"),
        )
        .expect("prepare");
        oms.reconcile(std::slice::from_ref(&orphan), 3);
        assert!(
            oms.reconcile(std::slice::from_ref(&orphan), 4)
                .cancel_orphans
                .is_empty()
        );
    }

    #[test]
    fn restart_never_resends_and_resolves_from_rest() {
        let dir = tempfile::tempdir().expect("tmp");
        let tid = TournamentId::new("t");
        let now = to_ns(t0());
        {
            let mut oms = open_oms(dir.path());
            oms.prepare(
                &OrderBundle::single(spec("e:1:1", "8", Side::Yes, 2, 5000)),
                t0(),
                &calendar(),
                &tid,
            )
            .expect("prepare");
            assert!(oms.mark_sent(&["e:1:1".into()], now));
            // Crash here: outcome never recorded.
        }
        let mut oms = open_oms(dir.path());
        assert_eq!(oms.get("e:1:1").map(|t| t.state), Some(OrderState::Unknown));
        let r = oms.reconcile(&[], now + 1);
        assert!(r.resend.is_empty(), "restored intents are never resent");
        let r = oms.reconcile(&[open_order(55, "8", 2.0)], now + 2);
        assert!(r.cancel_orphans.is_empty());
        let r = oms.reconcile(&[open_order(55, "8", 2.0)], now + 3);
        assert_eq!(
            r.cancel_orphans,
            vec![OrderId(55)],
            "the possibly-ours resting order is cancelled"
        );
    }

    #[test]
    fn cancel_before_ack_is_remembered() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut oms = open_oms(dir.path());
        oms.prepare(
            &OrderBundle::single(spec("f:1:1", "9", Side::Yes, 2, 5000)),
            t0(),
            &calendar(),
            &TournamentId::new("t"),
        )
        .expect("prepare");
        assert!(oms.mark_sent(&["f:1:1".into()], 1));
        assert!(oms.request_cancel(|_| true).is_empty());
        oms.apply_ack("f:1:1", &ack(12, "9", true, 0.0), 2);
        let r = oms.reconcile(&[open_order(12, "9", 2.0)], 3);
        assert_eq!(r.cancel_requested, vec![OrderId(12)]);
        oms.apply_cancel("f:1:1", CancelOutcome::Cancelled, 4);
        assert_eq!(oms.get("f:1:1").map(|t| t.state), Some(OrderState::Cancelled));
        assert_eq!(oms.illegal_transitions(), 0);
    }
}
