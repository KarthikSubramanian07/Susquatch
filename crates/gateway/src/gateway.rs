//! The only write path to the exchange.
//!
//! Every order passes, in this order: kill → lease → mode → freeze → time gate → risk pre-check →
//! write-ahead intent (OMS, fsynced) → journal → send, with documented-safe retries using the same
//! idempotency key. The gates are re-checked before every retry. Cancels need only the lease (a kill
//! or a freeze must never stop us from cancelling).

use std::{path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use serde_json::json;
use susq_contracts::v1::JournalEntry;
use susq_core::{Calendar, Clock, Mode, OrderBundle, OrderId, TournamentId};
use susq_journal::Journal;
use susq_oms::{FillDelta, Oms, PrepareError, Prepared};
use susq_platform::{
    CancelAllResponse, CancelOutcome, CancelScope, PlatformAdapter, PlatformError, RetryClass,
};
use susq_risk::{PreTrade, Rejection, RiskContext, RiskDetail};
use tokio::task::JoinHandle;

use crate::{
    kill::KillSwitch,
    lease::Lease,
    mode::go_live_armed,
    retry::{Jitter, RetryPolicy},
};

/// Why a write did not happen.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Blocked {
    #[error("killed: {0}")]
    Killed(String),
    #[error("not the lease holder")]
    NoLease,
    #[error("live mode but ops/GO_LIVE is not armed")]
    LiveNotArmed,
    #[error("frozen: {0}")]
    Frozen(String),
    #[error("time gate")]
    TimeGate,
    #[error("risk: {0}")]
    Risk(#[from] Rejection),
    #[error("oms: {0}")]
    Prepare(#[from] PrepareError),
}

impl Blocked {
    /// Short metric / journal label for the gate that closed.
    pub fn label(&self) -> &'static str {
        match self {
            Blocked::Killed(_) => "killed",
            Blocked::NoLease => "no_lease",
            Blocked::LiveNotArmed => "live_not_armed",
            Blocked::Frozen(_) => "frozen",
            Blocked::TimeGate => "time_gate",
            Blocked::Risk(_) => "risk",
            Blocked::Prepare(_) => "prepare",
        }
    }
}

/// How one prepared request ended.
#[derive(Debug, Clone, PartialEq)]
pub enum SendOutcome {
    /// Accepted; position changes from immediate fills.
    Acked(Vec<FillDelta>),
    /// Terminal 4xx.
    Rejected(String),
    /// Outcome unknown after the retry budget; reconcile resolves it.
    Unknown(String),
    /// A gate closed before the first attempt; nothing was sent.
    Blocked(Blocked),
    /// The OMS refused to send (intent already closed).
    NotSendable,
}

/// Everything the gateway needs.
pub struct GatewayDeps {
    /// Rate-limited live platform, or the paper broker.
    pub writes: Arc<dyn PlatformAdapter>,
    pub mode: Mode,
    pub go_live_file: PathBuf,
    pub kill: KillSwitch,
    pub lease: Lease,
    pub calendar: Calendar,
    pub tournament: TournamentId,
    pub journal: Journal,
    pub clock: Arc<dyn Clock>,
    pub retry: RetryPolicy,
    pub oms: Arc<Mutex<Oms>>,
}

/// The write path. Share it as `Arc<Gateway>`.
pub struct Gateway {
    d: GatewayDeps,
    frozen: Mutex<Option<String>>,
    jitter: Mutex<Jitter>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("mode", &self.d.mode)
            .field("tournament", &self.d.tournament)
            .finish()
    }
}

impl Gateway {
    /// Build the gateway.
    pub fn new(deps: GatewayDeps) -> Arc<Self> {
        let seed = deps.clock.now_ns() as u64;
        Arc::new(Self {
            d: deps,
            frozen: Mutex::new(None),
            jitter: Mutex::new(Jitter::new(seed)),
        })
    }

    /// Trading mode.
    pub fn mode(&self) -> Mode {
        self.d.mode
    }

    /// The OMS (shared with reconcile).
    pub fn oms(&self) -> &Arc<Mutex<Oms>> {
        &self.d.oms
    }

    /// The kill switch.
    pub fn kill(&self) -> &KillSwitch {
        &self.d.kill
    }

    /// The lease.
    pub fn lease(&self) -> &Lease {
        &self.d.lease
    }

    /// Freeze or thaw new orders (risk breakers).
    pub fn set_frozen(&self, reason: Option<String>) {
        *self.frozen.lock() = reason;
    }

    /// Gates for new orders: kill → lease → mode → freeze.
    pub fn write_gate(&self) -> Result<(), Blocked> {
        if let Some(reason) = self.d.kill.reason() {
            return Err(Blocked::Killed(reason));
        }
        if !self.d.lease.holds() {
            return Err(Blocked::NoLease);
        }
        if self.d.mode == Mode::Live && !go_live_armed(&self.d.go_live_file) {
            return Err(Blocked::LiveNotArmed);
        }
        if let Some(reason) = self.frozen.lock().clone() {
            return Err(Blocked::Frozen(reason));
        }
        Ok(())
    }

    /// Gate for cancels: only the lease.
    pub fn cancel_gate(&self) -> Result<(), Blocked> {
        if self.d.lease.holds() {
            Ok(())
        } else {
            Err(Blocked::NoLease)
        }
    }

    /// Journal a strategy or engine event (`farquote_scan`, ...) under this producer.
    pub fn journal_event(&self, kind: &str, body: serde_json::Value) {
        self.event(kind, body, "");
    }

    fn event(&self, kind: &str, body: serde_json::Value, key: &str) {
        let entry = JournalEntry {
            ts_ns: self.d.clock.now_ns(),
            direction: "event".into(),
            method: kind.into(),
            body_json: body.to_string(),
            idempotency_key: key.into(),
            ..Default::default()
        };
        if !self.d.journal.try_append(entry) {
            metrics::counter!("susq_journal_dropped_total").increment(1);
        }
    }

    fn blocked(&self, b: Blocked, what: &serde_json::Value) -> Blocked {
        metrics::counter!("susq_gateway_blocked_total", "reason" => b.label()).increment(1);
        self.event("blocked", json!({ "reason": b.to_string(), "order": what }), "");
        b
    }

    /// A risk refusal is journaled and logged with what the caps saw, so a refused ladder or
    /// snipe can be explained from the journal alone (the bridge's `Decision.detail`).
    fn blocked_by_risk(&self, r: Rejection, what: &serde_json::Value, detail: &RiskDetail) -> Blocked {
        let b = Blocked::Risk(r);
        metrics::counter!("susq_gateway_blocked_total", "reason" => b.label()).increment(1);
        tracing::info!(
            reason = %b,
            equity = detail.equity,
            total = detail.total,
            market = detail.market,
            positions = detail.positions,
            resting = detail.resting,
            "order refused by risk"
        );
        self.event(
            "blocked",
            json!({ "reason": b.to_string(), "order": what, "detail": detail }),
            "",
        );
        b
    }

    /// Run every gate, risk-check and write-ahead the bundle, then send each request on its own task.
    /// Returns the send tasks (await them in tests; the live loop lets them run).
    pub fn submit(
        self: &Arc<Self>,
        bundle: &OrderBundle,
        risk: &PreTrade,
        ctx: &RiskContext<'_>,
    ) -> Result<Vec<JoinHandle<SendOutcome>>, Blocked> {
        let summary = json!(bundle);
        self.write_gate().map_err(|b| self.blocked(b, &summary))?;
        let now = self.d.clock.now();
        if !self.d.calendar.may_place(now) {
            return Err(self.blocked(Blocked::TimeGate, &summary));
        }
        risk.check_bundle(ctx, bundle, &self.d.tournament)
            .map_err(|r| self.blocked_by_risk(r, &summary, &risk.detail(ctx, bundle)))?;
        let prepared = self
            .d
            .oms
            .lock()
            .prepare(bundle, now, &self.d.calendar, &self.d.tournament)
            .map_err(|e| self.blocked(e.into(), &summary))?;
        metrics::counter!("susq_orders_submitted_total", "strategy" => strategy_label(bundle)).increment(1);
        Ok(prepared
            .into_iter()
            .map(|p| tokio::spawn(Arc::clone(self).send(p)))
            .collect())
    }

    /// Send (or resend, from reconcile) one prepared request with the retry policy.
    pub async fn send(self: Arc<Self>, p: Prepared) -> SendOutcome {
        let ids = p.intent_ids();
        let started = tokio::time::Instant::now();
        let mut attempt: u32 = 1;
        loop {
            if let Err(b) = self.pre_attempt_gate() {
                let now_ns = self.d.clock.now_ns();
                let mut oms = self.d.oms.lock();
                for id in &ids {
                    if attempt == 1 {
                        oms.abandon(id, "BLOCKED", now_ns);
                    } else {
                        oms.apply_unknown(id, "BLOCKED_DURING_RETRY", now_ns);
                    }
                }
                drop(oms);
                return if attempt == 1 {
                    SendOutcome::Blocked(b)
                } else {
                    SendOutcome::Unknown("BLOCKED_DURING_RETRY".into())
                };
            }
            if !self.d.oms.lock().mark_sent(&ids, self.d.clock.now_ns()) {
                return SendOutcome::NotSendable;
            }
            if self.d.mode == Mode::Paper {
                self.event("paper_request", paper_body(&p), p.key());
            }
            let result = self.call(&p).await;
            let now_ns = self.d.clock.now_ns();
            match result {
                Ok(acks) => {
                    if self.d.mode == Mode::Paper {
                        self.event("paper_response", json!(acks), p.key());
                    }
                    return self.apply_acks(&ids, &acks, now_ns).await;
                }
                Err(e) => {
                    let code = e.label().to_owned();
                    let class = e.retry_class();
                    let delay =
                        self.d
                            .retry
                            .next_delay(attempt, class, started.elapsed(), &mut self.jitter.lock());
                    if class == RetryClass::Terminal {
                        let mut oms = self.d.oms.lock();
                        ids.iter().for_each(|id| oms.apply_reject(id, &code, now_ns));
                        metrics::counter!("susq_orders_rejected_total", "code" => code.clone()).increment(1);
                        return SendOutcome::Rejected(code);
                    }
                    // Retryable or unknown: the engine may have it. Record that before waiting.
                    {
                        let mut oms = self.d.oms.lock();
                        ids.iter().for_each(|id| oms.apply_unknown(id, &code, now_ns));
                    }
                    let Some(d) = delay else {
                        metrics::counter!("susq_orders_unknown_total").increment(1);
                        return SendOutcome::Unknown(code);
                    };
                    metrics::counter!("susq_order_retries_total", "code" => code.clone()).increment(1);
                    tracing::warn!(key = p.key(), %code, attempt, ?d, "retrying with the same idempotency key");
                    tokio::time::sleep(d).await;
                    attempt += 1;
                }
            }
        }
    }

    fn pre_attempt_gate(&self) -> Result<(), Blocked> {
        self.write_gate()?;
        if self.d.calendar.may_place(self.d.clock.now()) {
            Ok(())
        } else {
            Err(Blocked::TimeGate)
        }
    }

    async fn call(&self, p: &Prepared) -> Result<Vec<susq_platform::OrderAck>, PlatformError> {
        match p {
            Prepared::Single { request, .. } => self.d.writes.place_order(request).await.map(|a| vec![a]),
            Prepared::MultiLeg { request, .. } => {
                let resp = self.d.writes.place_multi_leg(request).await?;
                let mut legs = resp.results;
                legs.sort_by_key(|r| r.index);
                Ok(legs.into_iter().map(|r| r.data).collect())
            }
        }
    }

    async fn apply_acks(&self, ids: &[String], acks: &[susq_platform::OrderAck], now_ns: i64) -> SendOutcome {
        let (deltas, cancel_now) = {
            let mut oms = self.d.oms.lock();
            let deltas: Vec<FillDelta> = ids
                .iter()
                .zip(acks)
                .filter_map(|(id, ack)| oms.apply_ack(id, ack, now_ns))
                .collect();
            let cancel_now: Vec<(String, OrderId)> = ids
                .iter()
                .filter_map(|id| {
                    let t = oms.get(id)?;
                    (t.cancel_requested && t.state.is_live()).then(|| Some((id.clone(), t.order_id()?)))?
                })
                .collect();
            (deltas, cancel_now)
        };
        if !cancel_now.is_empty() {
            self.cancel_orders(&cancel_now).await;
        }
        SendOutcome::Acked(deltas)
    }

    /// Cancel specific orders (retrying 503s). Returns how many are confirmed closed.
    pub async fn cancel_orders(&self, targets: &[(String, OrderId)]) -> usize {
        if let Err(b) = self.cancel_gate() {
            self.blocked(b, &json!({ "cancel": targets.len() }));
            return 0;
        }
        let mut closed = 0;
        for (intent_id, oid) in targets {
            match self.with_write_retries(|| self.d.writes.cancel_order(*oid)).await {
                Ok(outcome) => {
                    self.d
                        .oms
                        .lock()
                        .apply_cancel(intent_id, outcome, self.d.clock.now_ns());
                    closed += 1;
                }
                Err(e) => tracing::warn!(%oid, error = %e, "cancel failed; reconcile will retry"),
            }
        }
        closed
    }

    /// Cancel an order we do not track (orphans found by reconcile).
    pub async fn cancel_untracked(&self, oid: OrderId) -> bool {
        if self.cancel_gate().is_err() {
            return false;
        }
        matches!(
            self.with_write_retries(|| self.d.writes.cancel_order(oid)).await,
            Ok(CancelOutcome::Cancelled | CancelOutcome::AlreadyClosed)
        )
    }

    /// Cancel everything in the tournament. In-flight intents are flagged so they are cancelled the
    /// moment they are acked.
    pub async fn cancel_all(&self, reason: &str) -> Result<CancelAllResponse, Blocked> {
        self.cancel_gate()
            .map_err(|b| self.blocked(b, &json!({ "cancel_all": reason })))?;
        self.event("cancel_all", json!({ "reason": reason }), "");
        self.d.oms.lock().request_cancel(|_| true);
        let scope = CancelScope::default();
        match self
            .with_write_retries(|| self.d.writes.cancel_all(&self.d.tournament, &scope))
            .await
        {
            Ok(resp) => {
                metrics::counter!("susq_cancel_all_total").increment(1);
                Ok(resp)
            }
            Err(e) => {
                tracing::error!(error = %e, "cancel-all failed; reconcile keeps cancelling");
                Ok(CancelAllResponse {
                    http_status: e.status().unwrap_or(0),
                    ..Default::default()
                })
            }
        }
    }

    /// Up to 4 attempts for retryable failures (cancels are idempotent).
    async fn with_write_retries<T, F, Fut>(&self, call: F) -> Result<T, PlatformError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, PlatformError>>,
    {
        let mut attempt = 1;
        loop {
            match call().await {
                Err(e) if attempt < 4 => match e.retry_class() {
                    RetryClass::Retry { min_wait } => {
                        let backoff = Duration::from_millis(250 << attempt).max(min_wait.unwrap_or_default());
                        tokio::time::sleep(backoff.min(Duration::from_secs(30))).await;
                        attempt += 1;
                    }
                    _ => return Err(e),
                },
                other => return other,
            }
        }
    }
}

fn strategy_label(bundle: &OrderBundle) -> String {
    bundle
        .legs
        .first()
        .map(|l| l.strategy.clone())
        .unwrap_or_default()
}

fn paper_body(p: &Prepared) -> serde_json::Value {
    match p {
        Prepared::Single { request, .. } => json!(request),
        Prepared::MultiLeg { request, .. } => json!(request),
    }
}
