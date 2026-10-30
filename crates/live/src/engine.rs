//! The trading engine: one [`Engine::step`] per tick.
//!
//! Each step: apply realtime events → collect finished sends → renew the lease → honour a kill
//! (cancel-all once, then do nothing) → refresh due inputs (reconcile, portfolio, constraints,
//! targets, market map, books) → breakers → needle transitions → strategies → gateway.
//! Every write goes through the gateway; the engine never talks to order endpoints directly.
//!
//! The racer, sniper, market maker, scenario and election-night executors decide every tick; the
//! far quoter decides on its own cadence (`strategies.farquote.interval_s`), because one pass can
//! place hundreds of resting orders under the account's write budget.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use susq_book::{BookStore, L2Book};
use susq_bus::monitor::NeedleSnapshot;
use susq_contracts::v1::{self, MarketMap};
use susq_core::{
    Action, Clock, Config, ExchangeId, MarketId, MarketIndex, OrderBundle, Side, clock::duration_ns,
};
use susq_gateway::{Blocked, Gateway, LeaseStatus, PaperBroker, SendOutcome};
use susq_platform::{
    OrderStatusFilter, PlatformAdapter, PlatformError, PnlPeriod, all_orders, all_relationships,
    collateral::CollateralGraph,
    models::LeaderboardPeriod,
    realtime::{RealtimeEvent, ResyncReason, market_of_channel},
};
use susq_risk::{
    BreakerConfig, Breakers, BundleCost, ContenderCap, DepthProbe, PreTrade, RiskContext, RiskLimits,
};
use susq_strategies::{
    ConstraintsRacer, Decision, EnExecutor, FairQuote, FarQuoter, IntentView, MarketMaker, NeedleView,
    QuietPosterior, ScenarioExecutor, Snapshot, Sniper, Strategy,
    farquote::{self, ScanSummary},
};
use susq_targets::{FairOutcome, FairReader, ReaderConfig, RefreshOutcome, TargetsReader};
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    cash,
    enr_health::{self, EnrFeed},
    realtime::RealtimeFeed,
    world::{World, intent_views, live_orders, needle_budget_used},
};

const S: i64 = 1_000_000_000;
/// Books fetched per step at most (keeps reads inside the rate budget).
const BOOKS_PER_STEP: usize = 8;
/// Violations not re-confirmed for this long are dropped.
const VIOLATION_TTL_NS: i64 = 30 * S;
/// Relationships are re-read at least this often.
const RELATIONSHIPS_EVERY_NS: i64 = 3600 * S;
/// Fair quotes absent from the live file for this long are forgotten.
const FAIR_FORGET_NS: i64 = 3600 * S;
/// Market map re-read period.
const MARKET_MAP_EVERY_NS: i64 = 60 * S;
/// How often the wanted set of per-market realtime channels is recomputed.
const REALTIME_MARKETS_EVERY_NS: i64 = 60 * S;

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
    pub realtime: Option<RealtimeFeed>,
    pub needle: Option<watch::Receiver<NeedleSnapshot>>,
    /// The results pollers' health (`--election`), for the disputed / stale-feed gates.
    pub enr: Option<EnrFeed>,
    /// The pre-registered count-order table (EXP-006), loaded by [`crate::wiring::build`] when
    /// the election-night executor is enabled with its gate or lock-time rule on.
    pub count_order: Option<Arc<susq_core::count_order::CountOrder>>,
}

#[derive(Debug, Default)]
struct Due {
    reconcile: i64,
    constraints: i64,
    lease: i64,
    relationships: i64,
    market_map: i64,
    leaderboard: i64,
    realtime: i64,
    /// Next far-quoter pass (`strategies.farquote.interval_s`).
    farquote: i64,
}

/// The engine.
pub struct Engine {
    d: EngineDeps,
    pub world: World,
    risk: PreTrade,
    breakers: Breakers,
    targets: TargetsReader,
    fair: FairReader,
    racer: ConstraintsRacer,
    sniper: Sniper,
    mm: MarketMaker,
    scenario: ScenarioExecutor,
    en: EnExecutor,
    farquote: FarQuoter,
    sends: Vec<JoinHandle<SendOutcome>>,
    due: Due,
    kill_handled: bool,
    /// The last-order cancel-all already ran.
    gate_closed_handled: bool,
    needle_epoch: u64,
    market_map_mtime: Option<std::time::SystemTime>,
    /// ALL collateral model over the current relationships (used only when collateral sizing is on).
    collateral: CollateralCost,
    /// `[risk] use_collateral_buying_power` resolved at start (`"auto"` reads the day-one confirmation).
    collateral_sizing: bool,
    /// Latest realtime book version applied per exchange (older `books` items are stale).
    book_versions: HashMap<ExchangeId, u64>,
    /// The needle's markets in realtime-slot order, as of the last healthy needle with races. It
    /// outlives a needle outage so the slots don't churn away from the races and back.
    needle_slots: Vec<ExchangeId>,
}

/// How the gateway received one decision's bundles.
#[derive(Debug, Default)]
struct Outcome {
    accepted: usize,
    /// Gate label → bundles it refused.
    blocked: BTreeMap<&'static str, u32>,
    /// Notional of the refused bundles (it never got reserved).
    blocked_notional: f64,
}

/// A far-quoter scan summary counts what the strategy wanted to place; make it say what the
/// gateway accepted, with the refusals under `blocked` and their cash taken out of `reserved`.
fn amend_scan(body: &mut serde_json::Value, outcome: &Outcome) {
    if outcome.blocked.is_empty() {
        return;
    }
    body["placed"] = serde_json::json!(outcome.accepted);
    if let Some(reserved) = body["reserved"].as_f64() {
        body["reserved"] = serde_json::json!(((reserved - outcome.blocked_notional) * 100.0).round() / 100.0);
    }
    let blocked = &mut body["blocked"];
    if !blocked.is_object() {
        *blocked = serde_json::json!({});
    }
    for (label, n) in &outcome.blocked {
        let prior = blocked[label].as_u64().unwrap_or(0);
        blocked[label] = serde_json::json!(prior + u64::from(*n));
    }
}

/// Collateral-aware bundle cost for risk's buying-power check.
#[derive(Debug, Default)]
struct CollateralCost(CollateralGraph);

impl BundleCost for CollateralCost {
    fn cash_needed(&self, positions: &HashMap<ExchangeId, i64>, bundle: &OrderBundle) -> f64 {
        let fills: Vec<(ExchangeId, i64, f64)> = bundle
            .legs
            .iter()
            .map(|l| (l.exchange_id.clone(), l.yes_delta(), l.yes_space().1.prob()))
            .collect();
        self.0.effective_cost(positions, &fills)
    }
}

/// Fresh books as risk's depth probe for the contender cap (EXP-014). A stale or dirty book reads
/// as no depth, so it never qualifies.
#[derive(Debug)]
struct FreshDepth<'a> {
    books: &'a BookStore,
    now_ns: i64,
    max_age_ns: i64,
}

impl DepthProbe for FreshDepth<'_> {
    fn depth_near_touch(
        &self,
        exchange: &ExchangeId,
        side: Side,
        action: Action,
        window_pts: f64,
    ) -> Option<f64> {
        let book = self.books.fresh(exchange, self.now_ns, self.max_age_ns)?;
        Some(book.depth_near_touch(side, action, window_pts))
    }
}

impl Engine {
    /// Build the engine (no I/O yet; see [`Engine::bootstrap`]).
    pub fn new(d: EngineDeps) -> Self {
        let cfg = &d.cfg;
        let collateral_sizing = cfg
            .risk
            .use_collateral_buying_power
            .enabled(&d.home.join(&cfg.paths.state));
        let world = World {
            tid: d.tournament.clone(),
            slug: cfg.platform.tournament_slug.clone(),
            ..World::default()
        };
        let en = &cfg.strategies.en;
        let mut limits = RiskLimits::from_cfg(&cfg.risk)
            .with_needle_glitch_pts(en.en_glitch_pts)
            .with_needle_budget_frac(en.active_night_budget_frac());
        if en.contender_mode {
            // EXP-014: only ever on by an explicit config flip.
            tracing::warn!(
                budget = en.contender_budget_frac,
                market_frac = en.contender_market_frac,
                min_depth = en.contender_min_depth,
                loss_per_hour = en.contender_loss_per_hour_frac,
                "election-night contender mode is ON"
            );
            limits = limits.with_contender_cap(ContenderCap {
                market_frac: en.contender_market_frac,
                min_depth: en.contender_min_depth,
                window_pts: en.contender_depth_window_pts,
            });
        }
        Self {
            risk: PreTrade { limits },
            breakers: Breakers::new(BreakerConfig::from_cfg(&cfg.risk).with_contender(en)),
            targets: TargetsReader::new(ReaderConfig::from_core(cfg, &d.home)),
            fair: FairReader::new(
                d.home.join(&cfg.paths.fair_live),
                Duration::from_secs(cfg.fair.stale_after_s),
            ),
            racer: ConstraintsRacer::new(cfg.strategies.racer.clone())
                .with_collateral_buying_power(collateral_sizing),
            sniper: Sniper::new(cfg.strategies.sniper.clone()),
            mm: MarketMaker::new(cfg.strategies.mm.clone()),
            scenario: ScenarioExecutor::new(cfg.strategies.scenario.clone())
                .with_max_order_notional_frac(cfg.risk.max_order_notional_frac),
            en: match d.count_order.clone() {
                Some(table) => EnExecutor::new(cfg.strategies.en.clone()).with_count_order(table),
                None => EnExecutor::new(cfg.strategies.en.clone()),
            },
            farquote: FarQuoter::new(cfg.strategies.farquote.clone()),
            sends: Vec::new(),
            due: Due::default(),
            kill_handled: false,
            gate_closed_handled: false,
            needle_epoch: 0,
            market_map_mtime: None,
            collateral: CollateralCost::default(),
            collateral_sizing,
            book_versions: HashMap::new(),
            needle_slots: Vec::new(),
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
        self.refresh_targets(now);
        self.refresh_fair(now);
        self.refresh_realtime_markets(now);
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
        match self.d.gateway.write_gate() {
            Ok(()) => self.decide_and_act(now).await,
            Err(blocked) => self.skip_farquote(now, &blocked),
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
        let Some(feed) = self.d.realtime.as_mut() else {
            return;
        };
        let mut events = Vec::new();
        while let Some(ev) = feed.try_recv() {
            events.push(ev);
        }
        for ev in events {
            self.apply_realtime(ev, now);
        }
    }

    /// Markets that deserve a per-market channel, most important first: the needle's races (as of
    /// its last healthy view; the election-night executor trades only on fresh books, and there
    /// are more markets than slots), then the targets file, then everything else the strategies read books for
    /// (positions, violations, fresh fair values), then the books that moved most recently.
    fn realtime_market_candidates(&self, now: i64) -> Vec<MarketId> {
        let markets = &self.world.markets;
        let mut out: Vec<MarketId> = self
            .needle_slots
            .iter()
            .filter_map(|ex| markets.market_of(ex))
            .collect();
        if let Some(t) = self.targets.current().map(|t| t.file()) {
            out.extend(
                t.positions
                    .iter()
                    .filter_map(|p| markets.market_of(&ExchangeId::new(p.exchange_id.clone()))),
            );
        }
        let fair_max_age = i64::try_from(self.d.cfg.fair.stale_after_s)
            .unwrap_or(60)
            .saturating_mul(S);
        let targets = self.targets.current().map(|t| t.file());
        out.extend(
            self.world
                .exchanges_of_interest(targets, now, fair_max_age)
                .iter()
                .filter_map(|ex| markets.market_of(ex)),
        );
        let mut recent: Vec<&L2Book> = self.world.books.iter().collect();
        recent.sort_by(|a, b| {
            b.observed_at_ns
                .cmp(&a.observed_at_ns)
                .then_with(|| a.exchange_id.cmp(&b.exchange_id))
        });
        out.extend(recent.iter().filter_map(|b| markets.market_of(&b.exchange_id)));
        out
    }

    /// Tell the book store which exchanges sit on a connected realtime channel, so their clean
    /// books stay fresh for `[live] realtime_book_max_age_s` (gaps and resyncs mark them dirty).
    fn mark_live_books(&mut self) {
        let max_age_s = self.d.cfg.live.realtime_book_max_age_s;
        let connected = self.world.health.get("realtime").is_some_and(|h| h.state == "ok");
        let live: Vec<ExchangeId> = match self.d.realtime.as_ref() {
            Some(feed) if connected && max_age_s > 0 => feed
                .live_markets()
                .flat_map(|m| self.world.markets.exchanges_for_market(m).iter().cloned())
                .collect(),
            _ => Vec::new(),
        };
        self.world
            .books
            .set_live(live, duration_ns(std::time::Duration::from_secs(max_age_s)));
    }

    /// Keep the per-market realtime channels on the markets that matter (rebuilds are
    /// rate-limited inside the feed).
    fn refresh_realtime_markets(&mut self, now: i64) {
        self.due.realtime = now + REALTIME_MARKETS_EVERY_NS;
        let Some(cap) = self.d.realtime.as_ref().map(RealtimeFeed::cap) else {
            return;
        };
        let fresh = crate::world::needle_exchanges(self.world.needle.as_ref(), &self.world.markets);
        let needle_arrived = !fresh.is_empty() && self.needle_slots.is_empty();
        if !fresh.is_empty() {
            self.needle_slots = fresh;
        }
        let wanted = self.realtime_market_candidates(now);
        let Some(feed) = self.d.realtime.as_mut() else {
            return;
        };
        if needle_arrived {
            // Sessions built before the needle's races were known (at start, or after a restart on
            // the night) would otherwise wait out the rebuild limit with the races on REST.
            feed.expedite();
        }
        if let Some(channels) = feed.ensure(wanted, now) {
            let markets = feed.markets().len();
            self.world.event(format!(
                "realtime session built: {channels} channels ({markets} of {cap} market slots)"
            ));
        }
    }

    /// Every exchange of a market is suspect: mark their books dirty.
    fn mark_market_dirty(&mut self, market: &MarketId, now: i64) {
        let exchanges: Vec<ExchangeId> = self.world.markets.exchanges_for_market(market).to_vec();
        for ex in exchanges {
            self.world.books.mark_dirty(&ex, now);
        }
    }

    fn apply_realtime(&mut self, ev: RealtimeEvent, now: i64) {
        match ev {
            RealtimeEvent::MarketBatch { batch, .. } => {
                for d in &batch.book_dirty {
                    self.world.books.mark_dirty(&d.exchange_id, now);
                }
                // A versioned book in the batch is as good as a REST fetch made now: it clears the
                // dirty mark set above and saves the refetch. Older versions are ignored.
                for b in &batch.books {
                    let known = self.book_versions.get(&b.exchange_id).copied();
                    if b.version().zip(known).is_some_and(|(v, k)| v <= k) {
                        continue;
                    }
                    if let Some(v) = b.version() {
                        self.book_versions.insert(b.exchange_id.clone(), v);
                    }
                    self.world
                        .books
                        .apply_snapshot(L2Book::from_api(&b.to_order_book(), now));
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
                // A gap or rejoin on one market's channel only taints that market's books; anything
                // wider taints everything.
                let scoped = match &reason {
                    ResyncReason::SocketError { channel } | ResyncReason::RevisionGap { channel, .. } => {
                        market_of_channel(channel)
                    }
                    _ => None,
                };
                if !matches!(reason, ResyncReason::InitialSubscribe) {
                    self.world.event(format!("realtime resync: {reason:?}"));
                }
                match scoped {
                    Some(market) => self.mark_market_dirty(&market, now),
                    None => {
                        self.world.books.mark_all_dirty(now);
                        self.due.reconcile = 0;
                        self.due.constraints = 0;
                    }
                }
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
        self.refresh_fair(now);
        self.refresh_enr(now);
        self.refresh_needle(now);
        if now >= self.due.realtime {
            self.refresh_realtime_markets(now);
        }
        self.mark_live_books();
        self.world
            .violations
            .retain(|_, (_, at)| now - *at <= VIOLATION_TTL_NS);
        self.refresh_books(now).await;
        if let Some(paper) = self.d.paper.clone() {
            // Books installed within book_max_age_s (by a paper write or refresh_books) are fresh
            // enough; the rest are re-read stalest first, within the same budget as refresh_books.
            let min_age = duration_ns(std::time::Duration::from_secs(self.d.cfg.live.book_max_age_s));
            let budget = std::time::Duration::from_millis(self.d.cfg.live.book_refresh_budget_ms.max(1));
            if let Err(e) = paper.refresh_resting(min_age, budget).await {
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
            Ok(rels) => {
                self.collateral = CollateralCost(CollateralGraph::new(&rels));
                self.world.relationships = rels.into_iter().map(|r| (r.id.clone(), r)).collect();
            }
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
            Ok(p) => {
                if p.total_account_value.is_finite() && p.total_account_value > 0.0 {
                    self.world.equity = p.total_account_value;
                    self.breakers.record_equity(now, p.total_account_value);
                }
                // True cash (EXP-015): account value minus holdings, as the bridge's risk uses.
                cash::observe_pnl(&mut self.world, &p);
            }
            Err(e) => self.platform_error("pnl", &e, now),
        }
        // `myBalance` only until the first portfolio read (or while equity is still unknown).
        if !self.world.cash_from_portfolio || self.world.equity <= 0.0 {
            if let Ok(t) = account.tournament(&self.world.slug).await {
                cash::observe_balance(&mut self.world, t.my_balance);
            }
        }
    }

    fn refresh_targets(&mut self, now: i64) {
        match self.targets.refresh(now) {
            RefreshOutcome::Loaded(t) => {
                self.world.event(format!("targets loaded: {}", t.source()));
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

    /// Live fair values (sniper and market maker). Each quote carries the file's `produced_at_ns`,
    /// so strategies see it go stale on their own and cancel what they built on it. Exchanges that
    /// drop out of the file keep their last quote until it has aged out (so their orders are
    /// cancelled as stale rather than orphaned), then are forgotten.
    fn refresh_fair(&mut self, now: i64) {
        match self.fair.refresh(now) {
            FairOutcome::Loaded(_) => {
                if let Some(snap) = self.fair.current() {
                    for f in &snap.fair {
                        let ex = ExchangeId::new(f.exchange_id.clone());
                        let prev = self.world.fair.get(&ex).map(|q| q.p_yes);
                        let quote = FairQuote {
                            p_yes: f.p_yes,
                            observed_at_ns: snap.produced_at_ns,
                            prev_p_yes: prev,
                            source: f.source.clone(),
                        };
                        self.world.fair.insert(ex, quote);
                    }
                }
            }
            FairOutcome::Rejected(r) => self.world.event(format!("fair file rejected: {r}")),
            FairOutcome::Unchanged => {}
        }
        self.world
            .fair
            .retain(|_, q| now - q.observed_at_ns <= FAIR_FORGET_NS);
        self.world.put_health(self.fair.health(now));
    }

    /// The results pollers' health (`--election`): per-source tiles, disputed races and which
    /// races still have a live feed (see [`enr_health`]).
    fn refresh_enr(&mut self, now: i64) {
        let Some(feed) = self.d.enr.as_ref() else { return };
        let health = feed.health.borrow().clone();
        let stale_after = i64::try_from(self.d.cfg.strategies.en.feed_stale_s)
            .unwrap_or(180)
            .saturating_mul(S);
        let view = enr_health::assess(&health, &feed.race_sources, now, stale_after);
        for item in &view.health {
            self.world.put_health(item.clone());
        }
        if let Some(prev) = self.world.enr.replace(view) {
            let now_disputed = &self.world.enr.as_ref().expect("just set").disputed_races;
            let opened: Vec<String> = now_disputed.difference(&prev.disputed_races).cloned().collect();
            let resolved: Vec<String> = prev.disputed_races.difference(now_disputed).cloned().collect();
            for race in opened {
                self.world
                    .event(format!("results sources disagree on {race}: race disputed"));
            }
            for race in resolved {
                self.world.event(format!("dispute on {race} resolved"));
            }
        }
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
        // Races the needle has gone quiet on (past posterior_max_age_s, same healthy epoch): only
        // the executor's lock-time rule reads them (EXP-006), so they are passed only when it is on.
        let en = &self.d.cfg.strategies.en;
        let quiet_posteriors: HashMap<String, QuietPosterior> = if en.enabled && en.lock_rule_enabled {
            snap.quiet_posteriors(now)
                .map(|m| {
                    m.into_iter()
                        .map(|(k, v)| {
                            (
                                k.to_owned(),
                                QuietPosterior {
                                    posterior: v.value.clone(),
                                    produced_at_ns: v.produced_at_ns,
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        // Without the pollers (a drill replaying straight onto the bus) nothing gates races;
        // with them, a race whose mapped sources are all stale gets no new buys and a disputed
        // race is refused (the executor does both).
        let (disputed_races, stale_feed_races) = match self.world.enr.as_ref() {
            Some(enr) => (
                enr.disputed_races.clone(),
                posteriors
                    .keys()
                    .chain(quiet_posteriors.keys())
                    .filter(|race| !enr.live_feed_races.contains(*race))
                    .cloned()
                    .collect(),
            ),
            None => (HashSet::new(), HashSet::new()),
        };
        self.world.needle = Some(NeedleView {
            healthy,
            epoch: snap.epoch,
            posteriors,
            disputed_races,
            stale_feed_races,
            quiet_posteriors,
        });
        self.world.put_health(snap.health.clone());
    }

    async fn refresh_books(&mut self, now: i64) {
        let max_age = duration_ns(std::time::Duration::from_secs(self.d.cfg.live.book_max_age_s));
        let fair_max_age = i64::try_from(self.d.cfg.fair.stale_after_s)
            .unwrap_or(60)
            .saturating_mul(S);
        let targets = self.targets.current().map(|t| t.file());
        let interest = self.world.exchanges_of_interest(targets, now, fair_max_age);
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
        // Oct 9-11: eight throttled book reads held a step for over 120 s and the watchdog restarted
        // the trader eleven times. Waiting on the book throttle is cancel-safe, so stop at the budget.
        let budget = std::time::Duration::from_millis(self.d.cfg.live.book_refresh_budget_ms.max(1));
        let deadline = tokio::time::Instant::now() + budget;
        for ex in todo.take(BOOKS_PER_STEP) {
            let read = self.d.platform.orderbook(&self.world.tid, &ex, 50);
            match tokio::time::timeout_at(deadline, read).await {
                Err(_) => break,
                Ok(Ok(b)) => {
                    if let Some(paper) = &self.d.paper {
                        paper.install(&ex, &b);
                    }
                    self.world
                        .books
                        .apply_snapshot(L2Book::from_api(&b, self.d.clock.now_ns()));
                }
                Ok(Err(e)) => {
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

    fn farquote_interval_ns(&self) -> i64 {
        i64::try_from(self.d.cfg.strategies.farquote.interval_s)
            .unwrap_or(600)
            .saturating_mul(S)
    }

    /// Whether this tick is one of the far quoter's passes. Arming it here (rather than inside the
    /// pass) means a gate that closes mid-interval costs one pass, never a burst of catch-up passes.
    fn farquote_due(&mut self, now: i64) -> bool {
        if !self.farquote.cfg.enabled || now < self.due.farquote {
            return false;
        }
        self.due.farquote = now + self.farquote_interval_ns();
        true
    }

    /// The gate is closed (no lease, killed, frozen, not armed): a far-quoter pass that fell due is
    /// skipped and journaled as such, one note instead of hundreds of blocked orders.
    fn skip_farquote(&mut self, now: i64, blocked: &Blocked) {
        if !self.farquote_due(now) {
            return;
        }
        let reserved: f64 = intent_views(&self.d.gateway.oms().lock())
            .iter()
            .filter(|i| i.strategy == farquote::NAME)
            .map(IntentView::resting_notional)
            .sum();
        let equity = self.world.equity;
        let summary = ScanSummary {
            reserved: (reserved * 100.0).round() / 100.0,
            budget: ((self.farquote.cfg.reserve_frac * equity) * 100.0).round() / 100.0,
            equity: (equity * 100.0).round() / 100.0,
            blocked: BTreeMap::from([(blocked.label().to_owned(), 1)]),
            ..ScanSummary::default()
        };
        tracing::info!(reason = %blocked, "far quoter pass skipped: gate closed");
        self.d.gateway.journal_event(
            farquote::SCAN_EVENT,
            serde_json::to_value(summary).unwrap_or_default(),
        );
    }

    async fn decide_and_act(&mut self, now: i64) {
        let farquote_due = self.farquote_due(now);
        let decisions = {
            let oms = self.d.gateway.oms().lock();
            let intents = intent_views(&oms);
            let live = live_orders(&oms);
            let needle_used = needle_budget_used(&oms, &self.world.positions);
            drop(oms);
            let violations = self.world.violation_list();
            let marks = self.world.marks();
            let locked = self.locked();
            let ctx = self.risk_ctx(now, &marks, &live, &locked, needle_used, None);
            let cluster_utilization = self.risk.cluster_utilization(&ctx);
            let targets = self.targets.current().map(|t| t.file());
            let snap = Snapshot {
                now_ns: now,
                equity: self.world.equity,
                // 0 until the first tournament read, as in `risk_ctx`.
                cash: (self.world.cash > 0.0).then_some(self.world.cash),
                books: &self.world.books,
                book_max_age_ns: duration_ns(std::time::Duration::from_secs(self.d.cfg.live.book_max_age_s)),
                positions: self.world.positions.map(),
                marks: &marks,
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
            let mut strategies: Vec<&dyn Strategy> =
                vec![&self.racer, &self.sniper, &self.mm, &self.scenario, &self.en];
            if farquote_due {
                strategies.push(&self.farquote);
            }
            susq_strategies::run_all(&strategies, &snap)
        };
        for (name, decision) in decisions {
            self.act(name, decision, now).await;
        }
    }

    /// Carry out one strategy's decision: cancels, then bundles through the gateway, then its
    /// journal events amended with what the gateway actually accepted.
    async fn act(&mut self, name: &str, decision: Decision, now: i64) {
        for note in &decision.notes {
            // The election-night executor's notes are its refusal reasons (count-order band,
            // uncorroborated edge, stale feed, ...). They get their own target so a rehearsal or the
            // night can turn on exactly those (`RUST_LOG=info,susq_live::en=debug`) without every
            // other strategy's per-tick chatter.
            if name == susq_strategies::en_executor::NAME {
                tracing::debug!(target: "susq_live::en", strategy = name, note = %note);
            } else {
                tracing::debug!(strategy = name, note = %note);
            }
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
        let mut outcome = Outcome::default();
        for bundle in &decision.bundles {
            match self.submit(bundle, now) {
                Ok(()) => outcome.accepted += 1,
                Err(label) => {
                    *outcome.blocked.entry(label).or_default() += 1;
                    outcome.blocked_notional += bundle.legs.iter().map(|l| l.max_notional()).sum::<f64>();
                }
            }
        }
        for mut event in decision.events {
            if event.kind == farquote::SCAN_EVENT {
                amend_scan(&mut event.body, &outcome);
            }
            self.d.gateway.journal_event(event.kind, event.body);
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
        needle_used: f64,
        depth: Option<&'a dyn DepthProbe>,
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
            needle_used,
            // 0 until the first tournament read: leave buying power to the platform until then.
            cash: (self.world.cash > 0.0).then_some(self.world.cash),
            bundle_cost: self
                .collateral_sizing
                .then_some(&self.collateral as &dyn BundleCost),
            depth,
        }
    }

    /// Submit a bundle; `Err` carries the gate's label (`risk`, `no_lease`, `duplicate`, ...).
    fn submit(&mut self, bundle: &OrderBundle, now: i64) -> Result<(), &'static str> {
        match self.try_submit(bundle, now) {
            Ok(()) => Ok(()),
            // Re-deciding the same thing in the same second is expected, not a problem.
            Err(Blocked::Prepare(susq_oms::PrepareError::Duplicate(_))) => Err("duplicate"),
            Err(b) => {
                tracing::info!(strategy = bundle.legs.first().map(|l| l.strategy.as_str()), reason = %b, "order blocked");
                Err(b.label())
            }
        }
    }

    fn try_submit(&mut self, bundle: &OrderBundle, now: i64) -> Result<(), Blocked> {
        // Rebuild the live-order view per bundle so several bundles in one step share the caps.
        let (live, needle_used) = {
            let oms = self.d.gateway.oms().lock();
            (live_orders(&oms), needle_budget_used(&oms, &self.world.positions))
        };
        let marks = self.world.marks();
        let locked = self.locked();
        // The contender cap reads depth off the same fresh books the strategies decided on.
        let probe = FreshDepth {
            books: &self.world.books,
            now_ns: now,
            max_age_ns: duration_ns(Duration::from_secs(self.d.cfg.live.book_max_age_s)),
        };
        let ctx = self.risk_ctx(now, &marks, &live, &locked, needle_used, Some(&probe));
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

    /// The paper broker (paper mode only).
    pub fn paper(&self) -> Option<&Arc<PaperBroker>> {
        self.d.paper.as_ref()
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
            SendOutcome::Blocked(_) | SendOutcome::NotSendable | SendOutcome::Expired => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_scan_summary_is_amended_with_what_the_gateway_refused() {
        let mut body = json!({
            "inBand": 2, "wanted": 8, "resting": 0, "placed": 8, "cancelled": 0,
            "reserved": 4000.0, "budget": 15000.0, "equity": 100000.0, "blocked": {"stale": 1}
        });
        let untouched = body.clone();
        amend_scan(&mut body, &Outcome::default());
        assert_eq!(body, untouched, "nothing refused: nothing to amend");

        let outcome = Outcome {
            accepted: 5,
            blocked: BTreeMap::from([("risk", 2), ("stale", 1)]),
            blocked_notional: 1500.0,
        };
        amend_scan(&mut body, &outcome);
        assert_eq!(body["placed"], 5);
        assert_eq!(body["reserved"], 2500.0);
        assert_eq!(body["blocked"], json!({"stale": 2, "risk": 2}));
    }
}
