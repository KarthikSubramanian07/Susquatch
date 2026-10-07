//! The write path against the mock matching engine: gates, retries with the same key, and
//! no double-send under injected faults.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use susq_core::OrderId;
use susq_core::{
    Action, Calendar, Clock, ExchangeId, ManualClock, MarketIndex, Mode, OrderBundle, OrderSpec, Pips, Qty,
    Side, TournamentId,
};
use susq_gateway::{Blocked, Gateway, GatewayDeps, KillSwitch, Lease, LeaseConfig, RetryPolicy, SendOutcome};
use susq_journal::{Journal, JournalConfig};
use susq_oms::{Oms, OmsConfig, OrderState};
use susq_platform::{
    BatchRequest, BatchResponse, BulkPrices, CancelAllResponse, CancelOutcome, CancelScope, Collateral,
    Constraints, Fill, Leaderboard, LeaderboardPeriod, Market, MarketStatusFilter, MultiLegRequest,
    MultiLegResponse, OrderAck, OrderBook, OrderStatusFilter, Page, PlaceOrderRequest, PlatformAdapter,
    PlatformResult, Pnl, PnlPeriod, Positions, RealtimeToken, Relationship, Tournament,
    mock::{CALLER, Endpoint, Fault, MockConfig, MockPlatform},
};
use susq_risk::{PreTrade, RiskContext, RiskLimits};
use uuid::Uuid;

const TID: &str = "tid-1";

fn t0() -> DateTime<Utc> {
    "2026-10-02T12:00:00Z".parse().expect("ts")
}

fn calendar() -> Calendar {
    Calendar {
        open_utc: "2026-10-01T16:00:00Z".parse().expect("ts"),
        lock_utc: "2026-11-04T17:00:00Z".parse().expect("ts"),
        last_order_utc: "2026-11-04T16:55:00Z".parse().expect("ts"),
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    clock: ManualClock,
    mock: Arc<MockPlatform>,
    gw: Arc<Gateway>,
    markets: MarketIndex,
    calendar: Calendar,
    tid: TournamentId,
    root: std::path::PathBuf,
}

fn lease_for(root: &Path, holder: &str, pid: u32, clock: &ManualClock) -> Lease {
    Lease::new(
        LeaseConfig {
            path: root.join("ops/state/trader.lease"),
            ttl: Duration::from_secs(30),
            holder: holder.into(),
            host: "test".into(),
            pid,
        },
        Arc::new(clock.clone()),
    )
}

fn harness(mode: Mode) -> Harness {
    harness_with(mode, |_| Journal::discard())
}

fn harness_with(mode: Mode, journal: impl FnOnce(&Path) -> Journal) -> Harness {
    harness_writes(mode, journal, |mock, _| mock)
}

/// Like [`harness_with`], with the gateway's writes going through `wrap(mock, clock)`.
fn harness_writes(
    mode: Mode,
    journal: impl FnOnce(&Path) -> Journal,
    wrap: impl FnOnce(Arc<MockPlatform>, ManualClock) -> Arc<dyn PlatformAdapter>,
) -> Harness {
    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path().to_owned();
    let clock = ManualClock::at(t0());
    let arc_clock: Arc<dyn Clock> = Arc::new(clock.clone());
    let mock = Arc::new(MockPlatform::new(
        arc_clock.clone(),
        MockConfig {
            tournament_id: TID.into(),
            starting_balance: 10_000.0,
            ..MockConfig::default()
        },
    ));
    mock.add_market("m1", "Race", &[("7", "Yes")]);
    mock.seed_order(
        "maker",
        "7",
        Side::Yes,
        Action::Sell,
        50,
        Pips::new(5500).expect("valid"),
    );
    let lease = lease_for(&root, "rust-live", 1, &clock);
    lease.acquire().expect("lease");
    std::fs::create_dir_all(root.join("ops")).expect("mkdir");
    let oms = Oms::open(
        OmsConfig {
            namespace: Uuid::parse_str("6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99").expect("ns"),
            sent_timeout_ns: 30_000_000_000,
            max_resend_age_ns: 60_000_000_000,
        },
        &root.join("state/rust-live/intents.jsonl"),
        clock.now_ns(),
    )
    .expect("oms");
    let writes = wrap(mock.clone(), clock.clone());
    let gw = Gateway::new(GatewayDeps {
        writes,
        mode,
        go_live_file: root.join("ops/GO_LIVE"),
        kill: KillSwitch::new(root.join("ops/KILL")),
        lease,
        calendar: calendar(),
        tournament: TournamentId::new(TID),
        journal: journal(&root),
        clock: arc_clock,
        retry: RetryPolicy::default(),
        oms: Arc::new(Mutex::new(oms)),
    });
    Harness {
        _dir: dir,
        clock,
        mock,
        gw,
        markets: MarketIndex::default(),
        calendar: calendar(),
        tid: TournamentId::new(TID),
        root,
    }
}

fn risk() -> PreTrade {
    PreTrade {
        limits: RiskLimits {
            per_market_gross_frac: 0.5,
            total_gross_frac: 1.0,
            cluster_kind_frac: Default::default(),
            cluster_override_frac: Default::default(),
            max_order_notional_frac: 0.5,
            glitch_through_fair: 0.15,
            glitch_max_frac: 0.02,
            needle_glitch_through_fair: 0.40,
            needle_budget_frac: None,
        },
    }
}

fn bundle(id: &str, qty: i64, price: u32) -> OrderBundle {
    OrderBundle::single(OrderSpec {
        intent_id: id.into(),
        strategy: "test".into(),
        exchange_id: ExchangeId::new("7"),
        market_id: None,
        side: Side::Yes,
        action: Action::Buy,
        quantity: Qty(qty),
        price: Pips::new(price).expect("valid"),
        ttl: Duration::from_secs(60),
        needle_derived: false,
        fair_yes: None,
    })
}

impl Harness {
    fn submit(&self, b: &OrderBundle) -> Result<Vec<tokio::task::JoinHandle<SendOutcome>>, Blocked> {
        let positions = HashMap::new();
        let marks = HashMap::new();
        let locked = HashSet::new();
        let ctx = RiskContext {
            now: self.clock.now(),
            equity: 10_000.0,
            calendar: &self.calendar,
            tournament: &self.tid,
            positions: &positions,
            marks: &marks,
            live_orders: &[],
            markets: &self.markets,
            locked: &locked,
            glitch_used: 0.0,
            needle_used: 0.0,
            cash: None,
            bundle_cost: None,
        };
        self.gw.submit(b, &risk(), &ctx)
    }

    async fn submit_one(&self, b: &OrderBundle) -> SendOutcome {
        let mut handles = self.submit(b).expect("gates pass");
        assert_eq!(handles.len(), 1);
        handles.remove(0).await.expect("task")
    }

    fn state(&self, id: &str) -> Option<OrderState> {
        self.gw.oms().lock().get(id).map(|t| t.state)
    }
}

#[tokio::test(start_paused = true)]
async fn happy_path_places_once() {
    let h = harness(Mode::Paper);
    let out = h.submit_one(&bundle("t:1", 10, 5500)).await;
    assert!(
        matches!(out, SendOutcome::Acked(ref d) if d[0].yes_delta == 10),
        "{out:?}"
    );
    assert_eq!(h.mock.engine_orders_created(), 1);
    assert_eq!(h.mock.position_of(CALLER, "7"), 10);
    assert_eq!(h.state("t:1"), Some(OrderState::Filled));
}

#[tokio::test(start_paused = true)]
async fn retries_documented_codes_with_the_same_key_and_never_double_places() {
    let h = harness(Mode::Paper);
    h.mock.inject(
        Endpoint::PlaceOrder,
        Fault::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        },
    );
    h.mock.inject(Endpoint::PlaceOrder, Fault::UnknownButPlaced);
    h.mock.inject(Endpoint::PlaceOrder, Fault::Unavailable);
    h.mock
        .inject(Endpoint::PlaceOrder, Fault::Network { executed: true });
    let started = tokio::time::Instant::now();
    let out = h.submit_one(&bundle("t:2", 5, 5000)).await;
    assert!(matches!(out, SendOutcome::Acked(_)), "{out:?}");
    assert_eq!(h.mock.calls(Endpoint::PlaceOrder), 5);
    assert_eq!(
        h.mock.engine_orders_created(),
        1,
        "502/network retries replayed, not re-placed"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "Retry-After honoured"
    );
    assert_eq!(h.gw.oms().lock().get("t:2").map(|t| t.intent.attempts), Some(5));
}

#[tokio::test(start_paused = true)]
async fn request_in_flight_waits_at_least_90_seconds() {
    let h = harness(Mode::Paper);
    h.mock.inject(Endpoint::PlaceOrder, Fault::InFlight);
    let started = tokio::time::Instant::now();
    let out = h.submit_one(&bundle("t:3", 1, 5000)).await;
    assert!(matches!(out, SendOutcome::Acked(_)));
    assert!(
        started.elapsed() >= Duration::from_secs(90),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn terminal_errors_are_not_retried() {
    let h = harness(Mode::Paper);
    // 1 000 000 shares at 0.50 is far beyond the 10 000 balance: 400 from the engine.
    let mut big = bundle("t:4", 1_000, 5000);
    big.legs[0].quantity = Qty(1_000_000);
    let limits_ok = PreTrade {
        limits: RiskLimits {
            max_order_notional_frac: 1_000.0,
            total_gross_frac: 1_000.0,
            per_market_gross_frac: 1_000.0,
            ..risk().limits
        },
    };
    let positions = HashMap::new();
    let marks = HashMap::new();
    let locked = HashSet::new();
    let ctx = RiskContext {
        now: h.clock.now(),
        equity: 10_000.0,
        calendar: &h.calendar,
        tournament: &h.tid,
        positions: &positions,
        marks: &marks,
        live_orders: &[],
        markets: &h.markets,
        locked: &locked,
        glitch_used: 0.0,
        needle_used: 0.0,
        cash: None,
        bundle_cost: None,
    };
    let out =
        h.gw.submit(&big, &limits_ok, &ctx)
            .expect("gates")
            .remove(0)
            .await
            .expect("task");
    assert!(matches!(out, SendOutcome::Rejected(_)), "{out:?}");
    assert_eq!(h.mock.calls(Endpoint::PlaceOrder), 1);
    assert_eq!(h.state("t:4"), Some(OrderState::Rejected));
}

#[tokio::test(start_paused = true)]
async fn exhausted_retries_become_unknown_and_reconcile_resends_the_same_key() {
    let h = harness(Mode::Paper);
    for _ in 0..3 {
        h.mock.inject(Endpoint::PlaceOrder, Fault::Internal);
    }
    let out = h.submit_one(&bundle("t:5", 2, 5000)).await;
    assert_eq!(
        out,
        SendOutcome::Unknown("INTERNAL_ERROR".into()),
        "500 is not retried blindly"
    );
    assert_eq!(h.state("t:5"), Some(OrderState::Unknown));
    let report = h.gw.oms().lock().reconcile(&[], h.clock.now_ns());
    assert_eq!(report.resend.len(), 1);
    // The next two sends still fail (500), the third goes through: still exactly one order.
    let out = Arc::clone(&h.gw).send(report.resend[0].clone()).await;
    assert!(matches!(out, SendOutcome::Unknown(_)));
    let report = h.gw.oms().lock().reconcile(&[], h.clock.now_ns());
    let _ = Arc::clone(&h.gw).send(report.resend[0].clone()).await;
    let report = h.gw.oms().lock().reconcile(&[], h.clock.now_ns());
    let out = Arc::clone(&h.gw).send(report.resend[0].clone()).await;
    assert!(matches!(out, SendOutcome::Acked(_)), "{out:?}");
    assert_eq!(h.mock.engine_orders_created(), 1);
}

#[tokio::test(start_paused = true)]
async fn kill_file_blocks_writes_but_not_cancel_all() {
    let h = harness(Mode::Paper);
    let rest = h.submit_one(&bundle("t:6", 3, 5000)).await;
    assert!(matches!(rest, SendOutcome::Acked(_)));
    std::fs::write(h.root.join("ops/KILL"), "manual kill").expect("write");
    assert!(matches!(h.submit(&bundle("t:7", 1, 5000)), Err(Blocked::Killed(r)) if r == "manual kill"));
    let resp =
        h.gw.cancel_all("kill")
            .await
            .expect("cancel-all allowed while killed");
    assert_eq!(resp.cancelled, 1);
    assert!(h.mock.open_orders_of(CALLER).is_empty());
}

#[tokio::test(start_paused = true)]
async fn kill_during_retries_stops_them() {
    let h = harness(Mode::Paper);
    h.mock.inject(Endpoint::PlaceOrder, Fault::Unavailable);
    let mut handles = h.submit(&bundle("t:8", 1, 5000)).expect("gates");
    // Let the first attempt hit the 503, then kill while the task waits to retry.
    while h.mock.calls(Endpoint::PlaceOrder) == 0 {
        tokio::task::yield_now().await;
    }
    h.gw.kill().trigger("ops.kill");
    let out = handles.remove(0).await.expect("task");
    assert_eq!(out, SendOutcome::Unknown("BLOCKED_DURING_RETRY".into()));
    assert_eq!(h.mock.calls(Endpoint::PlaceOrder), 1);
}

#[tokio::test(start_paused = true)]
async fn lease_held_by_the_bridge_blocks_every_write() {
    let h = harness(Mode::Paper);
    h.gw.lease().release().expect("release");
    let bridge = lease_for(&h.root, "bridge", 99, &h.clock);
    bridge.acquire().expect("bridge takes it");
    assert!(matches!(
        h.gw.lease().acquire().expect("io"),
        susq_gateway::LeaseStatus::HeldBy(_)
    ));
    assert_eq!(h.submit(&bundle("t:9", 1, 5000)).err(), Some(Blocked::NoLease));
    assert!(matches!(h.gw.cancel_all("x").await, Err(Blocked::NoLease)));
    assert_eq!(
        h.mock.calls(Endpoint::PlaceOrder) + h.mock.calls(Endpoint::CancelAll),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn live_mode_needs_go_live_and_time_gate_applies() {
    let h = harness(Mode::Live);
    assert_eq!(
        h.submit(&bundle("t:10", 1, 5000)).err(),
        Some(Blocked::LiveNotArmed)
    );
    std::fs::write(h.root.join("ops/GO_LIVE"), "go live").expect("write");
    assert!(matches!(
        h.submit_one(&bundle("t:11", 1, 5000)).await,
        SendOutcome::Acked(_)
    ));
    h.clock.set_ns(susq_core::clock::to_ns(
        "2026-11-04T16:55:00Z".parse().expect("ts"),
    ));
    h.gw.lease().acquire().expect("renew");
    assert_eq!(h.submit(&bundle("t:12", 1, 5000)).err(), Some(Blocked::TimeGate));
}

#[tokio::test(start_paused = true)]
async fn freeze_and_risk_block_before_anything_is_written() {
    let h = harness(Mode::Paper);
    h.gw.set_frozen(Some("loss breaker".into()));
    assert!(matches!(
        h.submit(&bundle("t:13", 1, 5000)),
        Err(Blocked::Frozen(_))
    ));
    h.gw.set_frozen(None);
    // 0.5 × equity order cap = 5 000; 20 000 shares at 0.50 = 10 000.
    assert!(matches!(
        h.submit(&bundle("t:14", 20_000, 5000)),
        Err(Blocked::Risk(_))
    ));
    assert!(
        !h.gw.oms().lock().knows("t:14"),
        "a refused order leaves no intent behind"
    );
}

#[tokio::test(start_paused = true)]
async fn risk_refusals_are_journaled_with_what_the_caps_saw() {
    let threads = Mutex::new(None);
    let h = harness_with(Mode::Paper, |root| {
        let (journal, thread) = Journal::start(JournalConfig {
            dir: root.join("journal"),
            producer: "rust-live".into(),
            capacity: 64,
            redact: vec![],
        })
        .expect("journal");
        *threads.lock() = Some(thread);
        journal
    });
    assert!(matches!(
        h.submit(&bundle("t:15", 20_000, 5000)),
        Err(Blocked::Risk(_))
    ));
    let thread = threads.lock().take().expect("writer");
    drop(h.gw);
    thread.join().expect("thread").expect("io");
    let day = susq_journal::utc_day(t0().timestamp_nanos_opt().expect("ns"));
    let text = std::fs::read_to_string(susq_journal::journal_path(
        &h.root.join("journal"),
        &day,
        "rust-live",
    ))
    .expect("journal file");
    let blocked: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("json"))
        .filter(|e| e["method"] == "blocked")
        .map(|e| serde_json::from_str(e["bodyJson"].as_str().expect("body")).expect("body json"))
        .collect();
    assert_eq!(blocked.len(), 1, "{text}");
    let b = &blocked[0];
    assert!(
        b["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("risk: order notional")),
        "{b}"
    );
    assert_eq!(b["detail"]["equity"], 10_000.0);
    assert_eq!(b["detail"]["total"], 0.0);
    assert_eq!(b["detail"]["market"], 0.0);
    assert_eq!(b["detail"]["positions"], 0);
    assert_eq!(b["detail"]["resting"], 0);
    assert_eq!(b["order"]["legs"][0]["intent_id"], "t:15");
}

// ── Orders whose own expiry passes before the venue accepts them ─────────────────────────────────

/// Writes that take `delay` of engine time before reaching the venue: what a 60 s rate-limit pause
/// (or the paper broker's rate-limited book read) does to an order with a shorter time-to-live.
struct SlowWrites {
    inner: Arc<MockPlatform>,
    clock: ManualClock,
    delay: Duration,
}

#[async_trait::async_trait]
impl PlatformAdapter for SlowWrites {
    async fn tournament(&self, slug: &str) -> PlatformResult<Tournament> {
        self.inner.tournament(slug).await
    }
    async fn markets_page(
        &self,
        slug: &str,
        status: MarketStatusFilter,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Market>> {
        self.inner.markets_page(slug, status, cursor).await
    }
    async fn prices(&self, tid: &TournamentId, ids: &[ExchangeId]) -> PlatformResult<BulkPrices> {
        self.inner.prices(tid, ids).await
    }
    async fn orderbook(
        &self,
        tid: &TournamentId,
        exchange: &ExchangeId,
        depth: u32,
    ) -> PlatformResult<OrderBook> {
        self.inner.orderbook(tid, exchange, depth).await
    }
    async fn place_order(&self, req: &PlaceOrderRequest) -> PlatformResult<OrderAck> {
        self.clock.advance(self.delay);
        self.inner.place_order(req).await
    }
    async fn place_batch(&self, req: &BatchRequest) -> PlatformResult<BatchResponse> {
        self.clock.advance(self.delay);
        self.inner.place_batch(req).await
    }
    async fn place_multi_leg(&self, req: &MultiLegRequest) -> PlatformResult<MultiLegResponse> {
        self.clock.advance(self.delay);
        self.inner.place_multi_leg(req).await
    }
    async fn cancel_order(&self, id: OrderId) -> PlatformResult<CancelOutcome> {
        self.inner.cancel_order(id).await
    }
    async fn cancel_all(&self, tid: &TournamentId, scope: &CancelScope) -> PlatformResult<CancelAllResponse> {
        self.inner.cancel_all(tid, scope).await
    }
    async fn orders_page(
        &self,
        tid: &TournamentId,
        status: OrderStatusFilter,
        exchange: Option<&ExchangeId>,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<susq_platform::Order>> {
        self.inner.orders_page(tid, status, exchange, cursor).await
    }
    async fn positions(&self, slug: &str) -> PlatformResult<Positions> {
        self.inner.positions(slug).await
    }
    async fn pnl(&self, slug: &str, period: PnlPeriod) -> PlatformResult<Pnl> {
        self.inner.pnl(slug, period).await
    }
    async fn fills_page(&self, slug: &str, cursor: Option<&str>) -> PlatformResult<Page<Fill>> {
        self.inner.fills_page(slug, cursor).await
    }
    async fn leaderboard(
        &self,
        slug: &str,
        period: LeaderboardPeriod,
        limit: u32,
        offset: u32,
    ) -> PlatformResult<Leaderboard> {
        self.inner.leaderboard(slug, period, limit, offset).await
    }
    async fn relationships_page(
        &self,
        tid: &TournamentId,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Relationship>> {
        self.inner.relationships_page(tid, cursor).await
    }
    async fn constraints(
        &self,
        tid: &TournamentId,
        violations_only: bool,
        min_violation: f64,
    ) -> PlatformResult<Constraints> {
        self.inner.constraints(tid, violations_only, min_violation).await
    }
    async fn realtime_token(&self) -> PlatformResult<RealtimeToken> {
        self.inner.realtime_token().await
    }
    async fn collateral(&self, tid: &TournamentId) -> PlatformResult<Collateral> {
        self.inner.collateral(tid).await
    }
}

/// The Oct 7 paper run: a 60 s pause inside the call outlasted a 30 s order, the venue refused
/// the stale order, and eleven such refusals tripped the reject breaker. An order whose own expiry
/// passed before the venue accepted it is now a local expiry: closed, never a rejection.
#[tokio::test(start_paused = true)]
async fn an_order_that_expires_waiting_inside_the_call_is_expired_not_rejected() {
    let h = harness_writes(
        Mode::Paper,
        |_| Journal::discard(),
        |mock, clock| {
            Arc::new(SlowWrites {
                inner: mock,
                clock,
                delay: Duration::from_secs(61),
            })
        },
    );
    let mut b = bundle("t:exp", 10, 5500);
    b.legs[0].ttl = Duration::from_secs(30);
    let out = h.submit_one(&b).await;
    assert!(matches!(out, SendOutcome::Expired), "{out:?}");
    assert_eq!(
        h.mock.calls(Endpoint::PlaceOrder),
        1,
        "the venue saw it once and refused it"
    );
    assert_eq!(h.state("t:exp"), Some(OrderState::Expired));
}

/// A retry that would go out after the order's own expiry is not sent at all.
#[tokio::test(start_paused = true)]
async fn a_retry_past_the_order_expiry_is_not_sent() {
    let h = harness(Mode::Paper);
    h.mock.inject(Endpoint::PlaceOrder, Fault::Unavailable);
    let mut b = bundle("t:retry", 10, 5500);
    b.legs[0].ttl = Duration::from_secs(10); // shorter than the 30 s lease, which must stay valid
    let handle = h.submit(&b).expect("gates pass").remove(0);
    // Let the first attempt run and fail with 503, then let the order's own expiry pass.
    while h.mock.calls(Endpoint::PlaceOrder) == 0 {
        tokio::task::yield_now().await;
    }
    h.clock.advance(Duration::from_secs(15));
    let out = handle.await.expect("task");
    assert!(matches!(out, SendOutcome::Expired), "{out:?}");
    assert_eq!(h.mock.calls(Endpoint::PlaceOrder), 1, "no doomed retry");
    assert_eq!(h.state("t:retry"), Some(OrderState::Expired));
}

/// An order already within a second of its expiry before the first attempt never leaves.
#[tokio::test(start_paused = true)]
async fn an_order_at_its_expiry_before_the_first_attempt_is_never_sent() {
    let h = harness(Mode::Paper);
    let mut b = bundle("t:late", 10, 5500);
    b.legs[0].ttl = Duration::from_secs(10);
    let handle = h.submit(&b).expect("gates pass").remove(0);
    h.clock.advance(Duration::from_millis(9_500));
    let out = handle.await.expect("task");
    assert!(matches!(out, SendOutcome::Expired), "{out:?}");
    assert_eq!(h.mock.calls(Endpoint::PlaceOrder), 0);
    assert_eq!(
        h.state("t:late"),
        Some(OrderState::Cancelled),
        "never sent: cancelled, not expired at the venue"
    );
}
