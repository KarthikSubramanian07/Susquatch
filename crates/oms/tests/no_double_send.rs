//! Property: over random interleavings of decisions, responses, failures, reconciles and restarts,
//! the engine never holds two orders for one intent, and every intent only ever uses one key.
//!
//! The fake engine below implements the documented idempotency contract: same key + same payload
//! replays the stored response; a different payload under a used key is a 409.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use chrono::{DateTime, Utc};
use proptest::prelude::*;
use susq_core::{
    Action, Calendar, ExchangeId, OrderBundle, OrderSpec, Pips, Qty, Side, TournamentId, clock::to_ns,
};
use susq_oms::{Oms, OmsConfig, Prepared};
use susq_platform::{Order, OrderAck, OrderRequest};
use uuid::Uuid;

#[derive(Debug, Clone)]
enum Op {
    /// A strategy decides intent `k` (small pool so re-decisions collide), atomic or not.
    Decide {
        k: u8,
        atomic: bool,
    },
    /// How the next send goes.
    Send(Outcome),
    Reconcile,
    Restart,
    Advance {
        secs: u8,
    },
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Ack,
    Reject,
    /// 502 ORDER_STATUS_UNKNOWN after the engine executed (or not).
    Unknown {
        executed: bool,
    },
}

#[derive(Default)]
struct Engine {
    by_key: HashMap<String, (String, Vec<OrderAck>)>,
    open: HashMap<i64, Order>,
    next_id: i64,
    created: usize,
    /// engine order ids created per intent id
    per_intent: HashMap<String, usize>,
    keys_per_intent: HashMap<String, HashSet<String>>,
}

impl Engine {
    fn execute(&mut self, p: &Prepared, now: DateTime<Utc>) -> Result<Vec<OrderAck>, u16> {
        let (key, legs, ids): (String, Vec<OrderRequest>, Vec<String>) = match p {
            Prepared::Single { intent_id, request } => (
                request.idempotency_key.clone(),
                vec![request.order.clone()],
                vec![intent_id.clone()],
            ),
            Prepared::MultiLeg {
                intent_ids, request, ..
            } => (
                request.idempotency_key.clone(),
                request.legs.clone(),
                intent_ids.clone(),
            ),
        };
        let fingerprint = format!("{legs:?}");
        for id in &ids {
            self.keys_per_intent
                .entry(id.clone())
                .or_default()
                .insert(key.clone());
        }
        if let Some((fp, acks)) = self.by_key.get(&key) {
            return if *fp == fingerprint {
                Ok(acks.clone())
            } else {
                Err(409)
            };
        }
        if legs.iter().any(|l| l.expiration_date <= now) {
            return Err(400);
        }
        let mut acks = Vec::new();
        for (leg, id) in legs.iter().zip(&ids) {
            self.next_id += 1;
            self.created += 1;
            *self.per_intent.entry(id.clone()).or_default() += 1;
            self.open.insert(
                self.next_id,
                Order {
                    id: self.next_id,
                    exchange_id: leg.exchange_id.clone(),
                    side: leg.side,
                    action: leg.action,
                    quantity: leg.quantity as f64,
                    price_limit: Some(leg.price.as_f64()),
                    open: true,
                    created_at: String::new(),
                    expiration_date: Some(leg.expiration_date.to_rfc3339()),
                },
            );
            acks.push(OrderAck {
                order_id: Some(self.next_id),
                exchange_id: leg.exchange_id.clone(),
                open: true,
                remaining_quantity: Some(leg.quantity as f64),
                action: None,
                side: None,
                price: None,
                quantity: None,
                terminal_reason_code: None,
                quantity_traded: 0.0,
                total_cost: 0.0,
                fill_price: None,
                all: None,
            });
        }
        self.by_key.insert(key, (fingerprint, acks.clone()));
        Ok(acks)
    }

    fn open_orders(&mut self, now: DateTime<Utc>) -> Vec<Order> {
        self.open.retain(|_, o| {
            o.expiration_date
                .as_deref()
                .and_then(|e| DateTime::parse_from_rfc3339(e).ok())
                .is_some_and(|e| e > now)
        });
        let mut v: Vec<Order> = self.open.values().cloned().collect();
        v.sort_by_key(|o| o.id);
        v
    }
}

fn cfg() -> OmsConfig {
    OmsConfig {
        namespace: Uuid::parse_str("6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99").expect("ns"),
        sent_timeout_ns: 5_000_000_000,
        max_resend_age_ns: 20_000_000_000,
    }
}

fn calendar() -> Calendar {
    Calendar {
        open_utc: "2026-10-01T16:00:00Z".parse().expect("ts"),
        lock_utc: "2026-11-04T17:00:00Z".parse().expect("ts"),
        last_order_utc: "2026-11-04T16:55:00Z".parse().expect("ts"),
    }
}

fn leg(k: u8, i: u8) -> OrderSpec {
    OrderSpec {
        intent_id: format!("racer:rel-{k}:1791475200"),
        strategy: "racer".into(),
        exchange_id: ExchangeId::new(format!("{}", u32::from(k) * 10 + u32::from(i))),
        market_id: None,
        side: Side::No,
        action: Action::Buy,
        quantity: Qty(5),
        price: Pips::new(4000).expect("valid"),
        ttl: Duration::from_secs(30),
        needle_derived: false,
        fair_yes: None,
    }
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0u8..4, any::<bool>()).prop_map(|(k, atomic)| Op::Decide { k, atomic }),
        4 => prop_oneof![
            Just(Outcome::Ack),
            Just(Outcome::Reject),
            any::<bool>().prop_map(|executed| Outcome::Unknown { executed }),
        ]
        .prop_map(Op::Send),
        2 => Just(Op::Reconcile),
        1 => Just(Op::Restart),
        1 => (1u8..40).prop_map(|secs| Op::Advance { secs }),
    ]
}

fn deliver(oms: &mut Oms, engine: &mut Engine, p: &Prepared, outcome: Outcome, now: DateTime<Utc>) {
    let ids = p.intent_ids();
    let now_ns = to_ns(now);
    if !oms.mark_sent(&ids, now_ns) {
        return;
    }
    match outcome {
        Outcome::Reject => ids
            .iter()
            .for_each(|id| oms.apply_reject(id, "VALIDATION_ERROR", now_ns)),
        Outcome::Unknown { executed } => {
            if executed {
                let _ = engine.execute(p, now);
            }
            ids.iter()
                .for_each(|id| oms.apply_unknown(id, "ORDER_STATUS_UNKNOWN", now_ns));
        }
        Outcome::Ack => match engine.execute(p, now) {
            Ok(acks) => {
                for (id, ack) in ids.iter().zip(&acks) {
                    oms.apply_ack(id, ack, now_ns);
                }
            }
            Err(code) => ids
                .iter()
                .for_each(|id| oms.apply_reject(id, &code.to_string(), now_ns)),
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, .. ProptestConfig::default() })]

    #[test]
    fn no_double_send(ops in proptest::collection::vec(op(), 1..60)) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("intents.jsonl");
        let tid = TournamentId::new("t-uuid");
        let mut now: DateTime<Utc> = "2026-10-08T16:00:00Z".parse().expect("ts");
        let mut oms = Oms::open(cfg(), &path, to_ns(now)).expect("open");
        let mut engine = Engine::default();
        let mut queue: Vec<Prepared> = Vec::new();

        for op in ops {
            match op {
                Op::Decide { k, atomic } => {
                    let legs = if atomic { vec![leg(k, 0), leg(k, 1)] } else { vec![leg(k, 0)] };
                    if let Ok(p) = oms.prepare(&OrderBundle { legs, atomic }, now, &calendar(), &tid) {
                        queue.extend(p);
                    }
                }
                Op::Send(outcome) => {
                    if let Some(p) = queue.pop() {
                        deliver(&mut oms, &mut engine, &p, outcome, now);
                    }
                }
                Op::Reconcile => {
                    let open = engine.open_orders(now);
                    let report = oms.reconcile(&open, to_ns(now));
                    for id in report.cancel_orphans.iter().chain(&report.cancel_requested) {
                        engine.open.remove(&id.0);
                    }
                    for p in report.resend {
                        deliver(&mut oms, &mut engine, &p, Outcome::Ack, now);
                    }
                }
                Op::Restart => {
                    drop(oms);
                    queue.clear();
                    oms = Oms::open(cfg(), &path, to_ns(now)).expect("reopen");
                }
                Op::Advance { secs } => now += chrono::Duration::seconds(i64::from(secs)),
            }
            // Invariant: at most one engine order per intent (leg), one key per intent.
            for (id, n) in &engine.per_intent {
                prop_assert!(*n <= 1, "intent {id} produced {n} engine orders");
            }
            for (id, keys) in &engine.keys_per_intent {
                prop_assert!(keys.len() == 1, "intent {id} used {} keys", keys.len());
            }
            prop_assert_eq!(oms.illegal_transitions(), 0);
        }
        // Everything the engine placed maps to a distinct intent.
        prop_assert_eq!(engine.created, engine.per_intent.len());
    }
}
