//! The trading engine: one [`Engine::step`] per tick.
//!
//! Each step: apply realtime events → collect finished sends → renew the lease → honour a kill
//! (cancel-all once, then do nothing) → refresh due inputs (reconcile, portfolio, constraints,
//! targets, market map, books) → breakers → needle transitions → strategies → gateway.
//! Every write goes through the gateway; the engine never talks to order endpoints directly.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use susq_book::L2Book;
use susq_bus::monitor::NeedleSnapshot;
use susq_contracts::v1::{self, MarketMap};
use susq_core::{Clock, Config, ExchangeId, MarketIndex, OrderBundle, clock::duration_ns};
use susq_gateway::{Blocked, Gateway, LeaseStatus, PaperBroker, SendOutcome};
use susq_platform::{
    OrderStatusFilter, PlatformAdapter, PlatformError, PnlPeriod, all_orders, all_relationships,
    models::LeaderboardPeriod,
    realtime::{RealtimeEvent, ResyncReason},
};
use susq_risk::{BreakerConfig, Breakers, PreTrade, RiskContext, RiskLimits};
use susq_strategies::{
    ConstraintsRacer, EnExecutor, FairQuote, MarketMaker, NeedleView, ScenarioExecutor, Snapshot, Sniper,
    Strategy,
};
use susq_targets::{ReaderConfig, RefreshOutcome, TargetsReader};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::world::{World, intent_views, live_orders};

const S: i64 = 1_000_000_000;
/// Books fetched per step at most (keeps reads inside the rate budget).
const BOOKS_PER_STEP: usize = 8;
/// Violations not re-confirmed for this long are dropped.
const VIOLATION_TTL_NS: i64 = 30 * S;
/// Relationships are re-read at least this often.
const RELATIONSHIPS_EVERY_NS: i64 = 3600 * S;
/// Market map re-read period.
const MARKET_MAP_EVERY_NS: i64 = 60 * S;

/// Everything the engine is built from.
pub struct EngineDeps {
    pub cfg: Config,
    /// `$SUSQ_HOME`: relative config paths resolve against it.
    pub home: PathBuf,
    pub clock: Arc<dyn Clock>,
    /// Market data and read endpoints (the rate-limited platform).
    pub platform: Arc<dyn PlatformAdapter>,
    /// Where our orders and positions live: the platform in live mode, the paper broker in paper.
    pub account: Arc<dyn PlatformAdapter>,
    /// Present in paper mode, to drive resting paper fills.
    pub paper: Option<Arc<PaperBroker>>,
    pub gateway: Arc<Gateway>,
    pub tournament: susq_core::TournamentId,
    pub realtime: Option<mpsc::Receiver<RealtimeEvent>>,
    pub needle: Option<watch::Receiver<NeedleSnapshot>>,
}

#[derive(Debug, Default)]
struct Due {
    reconcile: i64,
    constraints: i64,
    lease: i64,
    relationships: i64,
    market_map: i64,
    leaderboard: i64,
}

/// The engine.
pub struct Engine {
    d: EngineDeps,
    pub world: World,
    risk: PreTrade,
    breakers: Breakers,
    targets: TargetsReader,
    racer: ConstraintsRacer,
    sniper: Sniper,
    mm: MarketMaker,
    scenario: ScenarioExecutor,
    en: EnExecutor,
    sends: Vec<JoinHandle<SendOutcome>>,
    due: Due,
    kill_handled: bool,
    /// The last-order cancel-all already ran.
    gate_closed_handled: bool,
    needle_epoch: u64,
    market_map_mtime: Option<std::time::SystemTime>,
}

impl Engine {
    /// Build the engine (no I/O yet; see [`Engine::bootstrap`]).
    pub fn new(d: EngineDeps) -> Self {
        let cfg = &d.cfg;
        let world = World {
            tid: d.tournament.clone(),
            slug: cfg.platform.tournament_slug.clone(),
            ..World::default()
        };
        Self {
            risk: PreTrade {
                limits: RiskLimits::from_cfg(&cfg.risk),
            },
            breakers: Breakers::new(BreakerConfig::from_cfg(&cfg.risk)),
            targets: TargetsReader::new(ReaderConfig::from_core(cfg, &d.home)),
            racer: ConstraintsRacer::new(cfg.strategies.racer.clone()),
            sniper: Sniper::new(cfg.strategies.sniper.clone()),
            mm: MarketMaker::new(cfg.strategies.mm.clone()),
            scenario: ScenarioExecutor::new(cfg.strategies.scenario.clone()),
            en: EnExecutor::new(cfg.strategies.en.clone()),
            sends: Vec::new(),
            due: Due::default(),
            kill_handled: false,
            gate_closed_handled: false,
            needle_epoch: 0,
            market_map_mtime: None,
            world,
            d,
        }
    }

    /// The gateway.
    pub fn gateway(&self) -> &Arc<Gateway> {
        &self.d.gateway
    }

    /// Resolved config.
    pub fn cfg(&self) -> &Config {
        &self.d.cfg
    }

    fn path(&self, p: &Path) -> PathBuf {
        self.d.home.join(p)
    }

    /// First contact: lease, market map, relationships, portfolio and open orders from REST
    /// (no process trusts its own memory after a restart).
    pub async fn bootstrap(&mut self) {
        let now = self.d.clock.now_ns();
        self.renew_lease(now);
        self.load_market_map(now);
        self.refresh_relationships(now).await;
        self.reconcile(now).await;
        self.world.initial_equity = self.world.equity;
        self.world
            .event(format!("started in {} mode", self.d.gateway.mode().as_str()));
    }

    /// One tick.
    pub async fn step(&mut self) {
        let now = self.d.clock.now_ns();
        self.drain_realtime(now);
        self.collect_sends(now).await;
        if now >= self.due.lease {
            self.renew_lease(now);
        }
        if self.handle_kill().await || self.handle_last_order_gate(now).await {
            return;
        }
        self.refresh_inputs(now).await;
        self.apply_breakers(now).await;
        self.handle_needle(now).await;
        if self.d.gateway.write_gate().is_ok() {
            self.decide_and_act(now).await;
        }
    }

    /// Wait for every in-flight send (tests and shutdown).
    pub async fn settle(&mut self) {
        let now = self.d.clock.now_ns();
        for h in std::mem::take(&mut self.sends) {
            if let Ok(outcome) = h.await {
                self.on_send(outcome, now);
            }
        }
    }

    /// Cancel everything (lease holder only) and give the lease back.
    pub async fn shutdown(&mut self, reason: &str) {
        self.settle().await;
        if self.d.gateway.lease().holds() {
            let _ = self.d.gateway.cancel_all(reason).await;
            let _ = self.d.gateway.lease().release();
        }
        self.world.event(format!("shutdown: {reason}"));
    }

    // ───────────────────────────── lease & kill ─────────────────────────────

    fn renew_lease(&mut self, now: i64) {
        let renew_ns = i64::try_from(self.d.cfg.lease.renew_s)
            .unwrap_or(5)
            .saturating_mul(S);
        self.due.lease = now + renew_ns;
        match self.d.gateway.lease().acquire() {
            Ok(LeaseStatus::Held(l)) => self.world.set_health(
                "lease",
                "ok",
                format!("held until {} (epoch {})", l.expires_at_ns, l.epoch),
                now,
            ),
            Ok(LeaseStatus::HeldBy(l)) => self.world.set_health(
                "lease",
                "degraded",
                format!("held by {} (pid {}): read-only", l.holder, l.pid),
                now,
            ),
            Err(e) => self
                .world
                .set_health("lease", "down", format!("lease file error: {e}"), now),
        }
    }

    /// Returns true when killed (after making sure everything was cancelled once).
    async fn handle_kill(&mut self) -> bool {
        let Some(reason) = self.d.gateway.kill().reason() else {
            self.kill_handled = false;
            return false;
        };
        if !self.kill_handled {
            self.world.event(format!("KILL: {reason}"));
            if self
                .d
                .gateway
                .cancel_all(&format!("kill: {reason}"))
                .await
                .is_ok()
            {
                self.kill_handled = true;
            }
        }
        true
    }

    /// At the last-order gate (16:55Z Nov 4) cancel everything once; afterwards trade nothing.
    /// Returns true once the gate has closed.
    async fn handle_last_order_gate(&mut self, now: i64) -> bool {
        if susq_core::clock::from_ns(now) < self.d.cfg.calendar.last_order_utc {
            return false;
        }
        if !self.gate_closed_handled {
            self.world.event("last-order gate reached: cancelling everything");
            if self.d.gateway.cancel_all("last-order gate").await.is_ok() {
                self.gate_closed_handled = true;
            }
        }
        true
    }

    // ───────────────────────────── inputs ─────────────────────────────

    fn drain_realtime(&mut self, now: i64) {
        let Some(rx) = self.d.realtime.as_mut() else {
            return;
        };
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        for ev in events {
            self.apply_realtime(ev, now);
        }
    }

    fn apply_realtime(&mut self, ev: RealtimeEvent, now: i64) {
        match ev {
            RealtimeEvent::MarketBatch { batch, .. } => {
                for d in &batch.book_dirty {
                    self.world.books.mark_dirty(&d.exchange_id, now);
                }
                if !batch.market_settled.is_empty() {
                    self.due.reconcile = 0;
                }
            }
            RealtimeEvent::AccountBatch { .. } => self.due.reconcile = 0,
            RealtimeEvent::Violation { eval, .. } => {
                if eval.evaluation_status == "violated" {
                    self.world
                        .violations
                        .insert(eval.relationship_id.clone(), (*eval, now));
                }
            }
            RealtimeEvent::LeaderboardTick { .. } => self.due.leaderboard = 0,
            RealtimeEvent::Resync { reason } => {
                if !matches!(reason, ResyncReason::InitialSubscribe) {
                    self.world.event(format!("realtime resync: {reason:?}"));
                }
                self.world.books.mark_all_dirty(now);
                self.due.reconcile = 0;
                self.due.constraints = 0;
            }
            RealtimeEvent::Connected => self.world.set_health("realtime", "ok", "connected", now),
            RealtimeEvent::Disconnected { reason } => self.world.set_health(
                "realtime",
                "degraded",
                format!("disconnected: {reason}; polling"),
                now,
            ),
        }
    }

    async fn refresh_inputs(&mut self, now: i64) {
        if now >= self.due.market_map {
            self.load_market_map(now);
        }
        if now >= self.due.relationships {
            self.refresh_relationships(now).await;
        }
        if now >= self.due.reconcile {
            self.reconcile(now).await;
        }
        if now >= self.due.constraints {
            self.poll_constraints(now).await;
        }
        if now >= self.due.leaderboard {
            self.refresh_leaderboard(now).await;
        }
        self.refresh_targets(now);
        self.refresh_needle(now);
        self.world
            .violations
            .retain(|_, (_, at)| now - *at <= VIOLATION_TTL_NS);
        self.refresh_books(now).await;
        if let Some(paper) = self.d.paper.clone() {
            if let Err(e) = paper.refresh_resting().await {
                tracing::warn!(error = %e, "paper refresh failed");
            }
        }
    }

    fn load_market_map(&mut self, now: i64) {
        self.due.market_map = now + MARKET_MAP_EVERY_NS;
        let path = self.path(&self.d.cfg.paths.market_map);
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_some() && mtime == self.market_map_mtime {
            return;
        }
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| susq_contracts::from_json::<MarketMap>(&t).map_err(|e| e.to_string()))
        {
            Ok(map) => {
                self.world.markets = MarketIndex::new(&map);
                self.market_map_mtime = mtime;
                let verified = map.markets.iter().filter(|m| m.verified).count();
                self.world.set_health(
                    "market_map",
                    "ok",
                    format!("{} rows, {verified} verified", map.markets.len()),
                    now,
                );
            }
            Err(e) => self
                .world
                .set_health("market_map", "down", format!("{}: {e}", path.display()), now),
        }
    }

    async fn refresh_relationships(&mut self, now: i64) {
        self.due.relationships = now + RELATIONSHIPS_EVERY_NS;
        match all_relationships(self.d.platform.as_ref(), &self.world.tid).await {
            Ok(rels) => self.world.relationships = rels.into_iter().map(|r| (r.id.clone(), r)).collect(),
            Err(e) => self.platform_error("relationships", &e, now),
        }
    }

    async fn poll_constraints(&mut self, now: i64) {
        let every = (self.d.cfg.strategies.racer.poll_interval_s * 1e9) as i64;
        self.due.constraints = now + every.max(S);
        match self.d.platform.constraints(&self.world.tid, true, 0.01).await {
            Ok(c) => {
                self.world.violations.retain(|_, (_, at)| *at > now);
                for v in c.data.into_iter().filter(|v| v.evaluation_status == "violated") {
                    if !self.world.relationships.contains_key(&v.relationship_id) {
                        self.due.relationships = 0; // a new relationship appeared: re-read soon
                    }
                    self.world.violations.insert(v.relationship_id.clone(), (v, now));
                }
                self.world.set_health("platform", "ok", "", now);
            }
            Err(e) => self.platform_error("constraints", &e, now),
        }
    }

    async fn refresh_leaderboard(&mut self, now: i64) {
        let every = i64::try_from(self.d.cfg.telemetry.leaderboard_interval_s)
            .unwrap_or(3600)
            .saturating_mul(S);
        self.due.leaderboard = now + every;
        match self
            .d
            .platform
            .leaderboard(&self.world.slug, LeaderboardPeriod::All, 1, 0)
            .await
        {
            Ok(lb) => {
                self.world.rank = lb.my_rank;
                self.world.field_size = u32::try_from(lb.total).unwrap_or(0);
            }
            Err(e) => self.platform_error("leaderboard", &e, now),
        }
    }

    /// Repair local state from the account's open orders and portfolio.
    async fn reconcile(&mut self, now: i64) {
        let every = i64::try_from(self.d.cfg.oms.reconcile_interval_s)
            .unwrap_or(15)
            .saturating_mul(S);
        self.due.reconcile = now + every;
        let account = self.d.account.clone();
        let open = match all_orders(account.as_ref(), &self.world.tid, OrderStatusFilter::Open, None).await {
            Ok(o) => o,
            Err(e) => return self.platform_error("orders", &e, now),
        };
        let report = self.d.gateway.oms().lock().reconcile(&open, now);
        for id in report.cancel_orphans.iter().chain(&report.cancel_requested) {
            if self.d.gateway.cancel_untracked(*id).await {
                self.world.event(format!("cancelled untracked order {id}"));
            }
        }
        for p in report.resend {
            self.sends.push(tokio::spawn(Arc::clone(&self.d.gateway).send(p)));
        }
        match account.positions(&self.world.slug).await {
            Ok(p) => self.world.positions.set_from_rest(&p, now),
            Err(e) => self.platform_error("positions", &e, now),
        }
        match account.pnl(&self.world.slug, PnlPeriod::All).await {
            Ok(p) if p.total_account_value.is_finite() && p.total_account_value > 0.0 => {
                self.world.equity = p.total_account_value;
                self.breakers.record_equity(now, p.total_account_value);
            }
            Ok(_) => {}
            Err(e) => self.platform_error("pnl", &e, now),
        }
        if let Ok(t) = account.tournament(&self.world.slug).await {
            self.world.cash = t.my_balance;
            if self.world.equity <= 0.0 {
                self.world.equity = t.my_balance;
            }
        }
    }

    fn refresh_targets(&mut self, now: i64) {
        match self.targets.refresh(now) {
            RefreshOutcome::Loaded(t) => {
                self.world.event(format!("targets loaded: {}", t.source()));
                let fair: HashMap<ExchangeId, FairQuote> = t
                    .file()
                    .fair
                    .iter()
                    .map(|f| {
                        let ex = ExchangeId::new(f.exchange_id.clone());
                        let prev = self.world.fair.get(&ex).map(|q| q.p_yes);
                        let q = FairQuote {
                            p_yes: f.p_yes,
                            observed_at_ns: t.produced_at_ns(),
                            prev_p_yes: prev,
                            source: f.source.clone(),
                        };
                        (ex, q)
                    })
                    .collect();
                self.world.fair = fair;
                self.risk.limits.cluster_override_frac = t
                    .file()
                    .clusters
                    .iter()
                    .map(|c| (c.cluster.clone(), c.max_gross_frac))
                    .collect();
            }
            RefreshOutcome::Rejected(r) => self.world.event(format!("targets rejected: {r}")),
            RefreshOutcome::Unchanged => {}
        }
        self.world.put_health(self.targets.health(now));
    }

    fn refresh_needle(&mut self, now: i64) {
        let Some(rx) = self.d.needle.as_ref() else { return };
        let snap = rx.borrow().clone();
        self.world.needle_errors_per_min = snap.errors_per_min;
        let posteriors: HashMap<String, v1::RacePosterior> = snap
            .actionable_posteriors(now)
            .map(|m| {
                m.into_iter()
                    .map(|(k, v)| (k.to_owned(), v.value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let healthy = snap.actionable_posteriors(now).is_some();
        self.world.needle = Some(NeedleView {
            healthy,
            epoch: snap.epoch,
            posteriors,
        });
        self.world.put_health(snap.health.clone());
    }

    async fn refresh_books(&mut self, now: i64) {
        let max_age = duration_ns(std::time::Duration::from_secs(self.d.cfg.live.book_max_age_s));
        let interest = self
            .world
            .exchanges_of_interest(self.targets.current().map(|t| t.file()));
        // Dirty books first (realtime said they moved), then books about to go stale.
        let dirty: Vec<ExchangeId> = self
            .world
            .books
            .dirty_ids()
            .into_iter()
            .filter(|e| interest.contains(e))
            .collect();
        let aging = interest
            .iter()
            .filter(|e| !dirty.contains(e) && self.world.books.fresh(e, now, max_age / 2).is_none())
            .cloned()
            .collect::<Vec<_>>();
        let todo = dirty.into_iter().chain(aging);
        for ex in todo.take(BOOKS_PER_STEP) {
            match self.d.platform.orderbook(&self.world.tid, &ex, 50).await {
                Ok(b) => self
                    .world
                    .books
                    .apply_snapshot(L2Book::from_api(&b, self.d.clock.now_ns())),
                Err(e) => {
                    self.platform_error("orderbook", &e, now);
                    break;
                }
            }
        }
    }

    fn platform_error(&mut self, what: &str, e: &PlatformError, now: i64) {
        tracing::warn!(what, error = %e, "platform read failed");
        if e.code() == Some("TERMS_NOT_ACKNOWLEDGED") {
            self.world
                .action_needed
                .insert("Accept the current Terms & Rules on the platform".into());
        }
        if matches!(
            e.code(),
            Some("INVALID_API_KEY" | "API_KEY_REVOKED" | "API_KEY_EXPIRED" | "MISSING_API_KEY")
        ) {
            self.world
                .action_needed
                .insert(format!("API key problem: {}", e.label()));
        }
        self.world
            .set_health("platform", "degraded", format!("{what}: {e}"), now);
    }

    // ───────────────────────────── safety ─────────────────────────────

    async fn apply_breakers(&mut self, now: i64) {
        self.breakers
            .record_bus_errors(now, self.world.needle_errors_per_min);
        if let Some(trip) = self.breakers.take_trip() {
            self.world
                .event(format!("BREAKER {:?}: {}", trip.kind, trip.reason));
            let _ = self
                .d
                .gateway
                .cancel_all(&format!("breaker: {}", trip.reason))
                .await;
        }
        let frozen = self.breakers.frozen(now).map(|t| t.reason.clone());
        let state = if frozen.is_some() { "degraded" } else { "ok" };
        self.world
            .set_health("risk", state, frozen.clone().unwrap_or_default(), now);
        self.d.gateway.set_frozen(frozen);
    }

    async fn handle_needle(&mut self, _now: i64) {
        let Some(view) = self.world.needle.as_ref() else {
            return;
        };
        let went_unhealthy = view.epoch > self.needle_epoch;
        self.needle_epoch = view.epoch;
        if went_unhealthy || !view.healthy {
            let targets = self
                .d
                .gateway
                .oms()
                .lock()
                .request_cancel(|t| t.meta.needle_derived);
            if !targets.is_empty() {
                self.world.event(format!(
                    "needle unhealthy: cancelling {} needle-derived orders",
                    targets.len()
                ));
                self.d.gateway.cancel_orders(&targets).await;
            }
        }
    }

    // ───────────────────────────── decisions ─────────────────────────────

    async fn decide_and_act(&mut self, now: i64) {
        let decisions = {
            let oms = self.d.gateway.oms().lock();
            let intents = intent_views(&oms);
            let live = live_orders(&oms);
            drop(oms);
            let violations = self.world.violation_list();
            let marks = self.world.marks();
            let locked = self.locked();
            let ctx = self.risk_ctx(now, &marks, &live, &locked);
            let cluster_utilization = self.risk.cluster_utilization(&ctx);
            let targets = self.targets.current().map(|t| t.file());
            let snap = Snapshot {
                now_ns: now,
                equity: self.world.equity,
                books: &self.world.books,
                book_max_age_ns: duration_ns(std::time::Duration::from_secs(self.d.cfg.live.book_max_age_s)),
                positions: self.world.positions.map(),
                intents: &intents,
                markets: &self.world.markets,
                fair: &self.world.fair,
                fair_cfg: &self.d.cfg.fair,
                targets,
                targets_increase_allowed: self.targets.increase_exposure_allowed(),
                needle: self.world.needle.as_ref(),
                relationships: &self.world.relationships,
                violations: &violations,
                cluster_utilization: &cluster_utilization,
            };
            let strategies: [&dyn Strategy; 5] =
                [&self.racer, &self.sniper, &self.mm, &self.scenario, &self.en];
            susq_strategies::run_all(&strategies, &snap)
        };
        for (name, decision) in decisions {
            for note in &decision.notes {
                tracing::debug!(strategy = name, note = %note);
            }
            if !decision.cancels.is_empty() {
                let ids: HashSet<&str> = decision.cancels.iter().map(|c| c.intent_id.as_str()).collect();
                let targets = self
                    .d
                    .gateway
                    .oms()
                    .lock()
                    .request_cancel(|t| ids.contains(t.intent.intent_id.as_str()));
                self.d.gateway.cancel_orders(&targets).await;
            }
            for bundle in decision.bundles {
                self.submit(&bundle, now);
            }
        }
    }

    fn locked(&self) -> HashSet<ExchangeId> {
        self.targets
            .current()
            .map(|t| t.locked_exchanges().map(ExchangeId::new).collect())
            .unwrap_or_default()
    }

    fn risk_ctx<'a>(
        &'a self,
        now: i64,
        marks: &'a HashMap<ExchangeId, f64>,
        live: &'a [susq_risk::LiveOrder],
        locked: &'a HashSet<ExchangeId>,
    ) -> RiskContext<'a> {
        RiskContext {
            now: susq_core::clock::from_ns(now),
            equity: self.world.equity,
            calendar: &self.d.cfg.calendar,
            tournament: &self.world.tid,
            positions: self.world.positions.map(),
            marks,
            live_orders: live,
            markets: &self.world.markets,
            locked,
            glitch_used: self.world.glitch_used,
        }
    }

    fn submit(&mut self, bundle: &OrderBundle, now: i64) {
        match self.try_submit(bundle, now) {
            Ok(()) | Err(Blocked::Prepare(susq_oms::PrepareError::Duplicate(_))) => {}
            Err(b) => {
                tracing::info!(strategy = bundle.legs.first().map(|l| l.strategy.as_str()), reason = %b, "order blocked")
            }
        }
    }

    fn try_submit(&mut self, bundle: &OrderBundle, now: i64) -> Result<(), Blocked> {
        // Rebuild the live-order view per bundle so several bundles in one step share the caps.
        let live = live_orders(&self.d.gateway.oms().lock());
        let marks = self.world.marks();
        let locked = self.locked();
        let ctx = self.risk_ctx(now, &marks, &live, &locked);
        let glitch: f64 = bundle
            .legs
            .iter()
            .filter(|l| self.risk.is_glitch(l))
            .map(|l| l.max_notional())
            .sum();
        let handles = self.d.gateway.submit(bundle, &self.risk, &ctx)?;
        self.world.glitch_used += glitch;
        self.sends.extend(handles);
        Ok(())
    }

    /// Submit one bundle outside the strategy loop (the `smoke` command). Same gates and checks.
    pub fn place(&mut self, bundle: &OrderBundle) -> Result<(), Blocked> {
        let now = self.d.clock.now_ns();
        self.try_submit(bundle, now)
    }

    /// The account adapter (paper broker in paper mode).
    pub fn account(&self) -> &Arc<dyn PlatformAdapter> {
        &self.d.account
    }

    /// The read platform.
    pub fn platform(&self) -> &Arc<dyn PlatformAdapter> {
        &self.d.platform
    }

    async fn collect_sends(&mut self, now: i64) {
        let (done, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sends)
            .into_iter()
            .partition(|h| h.is_finished());
        self.sends = pending;
        for h in done {
            if let Ok(outcome) = h.await {
                self.on_send(outcome, now);
            }
        }
    }

    fn on_send(&mut self, outcome: SendOutcome, now: i64) {
        match outcome {
            SendOutcome::Acked(deltas) => {
                for d in deltas {
                    self.world.positions.apply(&d);
                    self.world.trade(v1::TradeSummary {
                        ts_ns: now,
                        strategy: String::new(),
                        title: d.exchange_id.to_string(),
                        side: if d.yes_delta > 0 {
                            v1::Side::Yes as i32
                        } else {
                            v1::Side::No as i32
                        },
                        action: v1::Action::Buy as i32,
                        quantity: d.yes_delta.abs(),
                        price: 0.0,
                    });
                }
            }
            SendOutcome::Rejected(code) => {
                self.breakers.record_reject(now);
                if code == "TERMS_NOT_ACKNOWLEDGED" {
                    self.world
                        .action_needed
                        .insert("Accept the current Terms & Rules on the platform".into());
                }
            }
            SendOutcome::Unknown(code) => self
                .world
                .event(format!("order outcome unknown ({code}); reconciling")),
            SendOutcome::Blocked(_) | SendOutcome::NotSendable => {}
        }
    }

    /// Exchanges with a live intent (for status).
    pub fn live_exchanges(&self) -> BTreeSet<ExchangeId> {
        self.d
            .gateway
            .oms()
            .lock()
            .live()
            .map(|t| t.exchange_id())
            .collect()
    }

    /// Whether increases are currently allowed by the targets reader.
    pub fn targets_increase_allowed(&self) -> bool {
        self.targets.increase_exposure_allowed()
    }

    /// Current breaker trips (for status).
    pub fn breaker_history(&self) -> &[susq_risk::Trip] {
        self.breakers.history()
    }
}
