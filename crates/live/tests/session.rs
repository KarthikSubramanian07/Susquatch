//! A simulated session of the full live loop in paper mode against the mock platform:
//! the racer finds and trades a seeded violation, a kill file mid-run cancels everything,
//! a lease held by the bridge prevents any write, stale targets stop new exposure, and the far
//! quoter rests, keeps, trims and pulls its ladders on its own cadence (and recognises them after
//! a restart, in live mode against the mock's matching engine).

use std::{path::Path, sync::Arc, time::Duration};

use parking_lot::Mutex;
use serde_json::json;
use susq_contracts::v1::{
    self, Header, MarketMap, MarketRule, Producer, TargetFile, TargetPointer, TargetPosition,
};
use susq_core::{Action, Clock, Config, ExchangeId, ManualClock, MarketId, Mode, Pips, Side, TournamentId};
use susq_gateway::{Lease, LeaseConfig, LeaseStatus};
use susq_journal::Journal;
use susq_live::{
    engine::Engine,
    realtime::RealtimeFeed,
    status,
    wiring::{BuildOpts, build},
};
use susq_oms::OrderState;
use susq_platform::{
    BatchRequest, BatchResponse, BulkPrices, CancelAllResponse, CancelOutcome, CancelScope, Collateral,
    Constraints, Fill, Leaderboard, LeaderboardPeriod, Market, MarketStatusFilter, MultiLegRequest,
    MultiLegResponse, OrderAck, OrderBook, OrderStatusFilter, Page, PlaceOrderRequest, PlatformAdapter,
    PlatformResult, Pnl, PnlPeriod, Positions, RealtimeToken, Relationship, Tournament, all_orders,
    mock::{CALLER, MockConfig, MockPlatform},
    realtime::{BookDirty, MarketBatch, RealtimeEvent, ResyncReason, RtBook, RtLevel},
};

const TID: &str = "tid-1";
const SLUG: &str = "midterm-elections";
const S: i64 = 1_000_000_000;

fn px(raw: u32) -> Pips {
    Pips::new(raw).expect("valid")
}

fn config() -> Config {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/susq.toml"))
        .expect("shared config");
    let mut cfg = Config::from_toml_str(&text).expect("valid config");
    cfg.realtime.enabled = false;
    // These sessions start on 2026-10-08 and exercise the scenario executor from the first tick; the
    // shipped start date is an experiment knob (EXP-002, EXP-007 moved it past this clock), not what
    // is under test here.
    cfg.strategies.scenario.start_utc = None;
    // Sessions run in a temporary $SUSQ_HOME: read the shipped count-order table from the repo.
    cfg.strategies.en.count_order_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/count_order.toml");
    cfg
}

/// A seeded exchange: three mutually exclusive legs whose YES bids sum to 1.05, a House leg, and
/// two near-certain governor legs (the Cup prices one at ~0.02 and the other at ~0.98).
fn seed(mock: &MockPlatform) {
    mock.add_market(
        "m1",
        "Who wins OH-SEN",
        &[("1", "DEM"), ("2", "REP"), ("3", "IND")],
    );
    mock.add_market("m2", "House control", &[("4", "DEM")]);
    mock.add_market("m3", "TX-GOV", &[("5", "DEM")]);
    mock.add_market("m4", "CA-GOV", &[("6", "DEM")]);
    for (ex, bid, ask) in [
        ("1", 4000, 4500),
        ("2", 3500, 4000),
        ("3", 3000, 3500),
        ("4", 5000, 5500),
        ("5", 100, 300),
        ("6", 9700, 9900),
    ] {
        mock.seed_order("alice", ex, Side::Yes, Action::Buy, 100, px(bid));
        mock.seed_order("bob", ex, Side::Yes, Action::Sell, 100, px(ask));
    }
    let node = |ex: &str| json!({"exchangeId": ex, "marketId": "m1", "role": "member"});
    mock.set_relationships(vec![
        serde_json::from_value(json!({
            "id": "rel-9", "type": "mutually_exclusive", "status": "active", "isExhaustive": false,
            "nodes": [node("1"), node("2"), node("3")]
        }))
        .expect("relationship"),
    ]);
    mock.set_constraints(vec![
        serde_json::from_value(json!({
            "relationshipId": "rel-9", "type": "mutually_exclusive", "violationAmount": 0.05,
            "evaluationStatus": "violated", "reason": "sum of YES bids exceeds 1",
            "suggestedCorrectiveTrades": [
                {"exchangeId": "1", "outcomeSide": "no", "action": "buy"},
                {"exchangeId": "2", "outcomeSide": "no", "action": "buy"},
                {"exchangeId": "3", "outcomeSide": "no", "action": "buy"}
            ]
        }))
        .expect("constraint"),
    ]);
}

fn map_rule(ex: &str, market: &str, race: &str, party: &str, clusters: &[&str]) -> MarketRule {
    MarketRule {
        market_id: market.into(),
        exchange_id: ex.into(),
        title: format!("{race} {party}"),
        race_id: race.into(),
        kind: "race_party".into(),
        party: party.into(),
        clusters: clusters.iter().map(|c| (*c).to_owned()).collect(),
        verified: true,
        ..Default::default()
    }
}

fn write_market_map(home: &Path, cfg: &Config) {
    write_market_map_plus(home, cfg, Vec::new());
}

fn write_market_map_plus(home: &Path, cfg: &Config, extra: Vec<MarketRule>) {
    let rule = map_rule;
    let mut markets = vec![
        rule("1", "m1", "2026-SEN-OH", "DEM", &["national", "state:OH"]),
        rule("2", "m1", "2026-SEN-OH", "REP", &["national", "state:OH"]),
        rule("3", "m1", "2026-SEN-OH", "IND", &["national", "state:OH"]),
        rule("4", "m2", "", "DEM", &["house"]),
        rule("5", "m3", "2026-GOV-TX", "DEM", &["state:TX"]),
        rule("6", "m4", "2026-GOV-CA", "DEM", &["state:CA"]),
    ];
    markets.extend(extra);
    let map = MarketMap {
        header: None,
        markets,
    };
    let path = home.join(&cfg.paths.market_map);
    std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
    std::fs::write(path, susq_contracts::to_json(&map).expect("json")).expect("write");
}

/// Targets asking for 200 YES on the House leg at ≤ 0.52 (the ask is 0.55, so it rests).
fn write_targets(home: &Path, produced_at_ns: i64, name: &str) {
    let mut tf = TargetFile {
        header: Some(Header {
            schema_version: susq_contracts::SCHEMA_VERSION,
            produced_at_ns,
            producer: Some(Producer {
                name: "python-scheduler".into(),
                git_sha: "test".into(),
            }),
            seq: 1,
        }),
        tournament_slug: SLUG.into(),
        valid_until_ns: produced_at_ns + 7 * 24 * 3600 * S,
        positions: vec![TargetPosition {
            exchange_id: "4".into(),
            market_id: "m2".into(),
            target_qty: 200,
            max_entry_price: 0.52,
            locked: false,
            tranche: "T1".into(),
            cluster: "house".into(),
            rationale: "test".into(),
        }],
        ..Default::default()
    };
    tf.content_sha256 = susq_contracts::target_content_sha256(&tf);
    let dir = home.join("targets");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join(name), susq_contracts::to_json(&tf).expect("json")).expect("write");
    let ptr = TargetPointer {
        header: tf.header.clone(),
        file: name.into(),
        content_sha256: tf.content_sha256.clone(),
    };
    std::fs::write(
        dir.join("current.json"),
        susq_contracts::to_json(&ptr).expect("json"),
    )
    .expect("write");
}

struct Session {
    home: tempfile::TempDir,
    clock: ManualClock,
    mock: Arc<MockPlatform>,
    engine: Engine,
}

async fn session(prepare: impl FnOnce(&Path, &ManualClock)) -> Session {
    session_with(|_| {}, prepare).await
}

async fn session_with(tweak: impl FnOnce(&mut Config), prepare: impl FnOnce(&Path, &ManualClock)) -> Session {
    session_full(tweak, prepare, None).await
}

async fn session_full(
    tweak: impl FnOnce(&mut Config),
    prepare: impl FnOnce(&Path, &ManualClock),
    realtime: Option<RealtimeFeed>,
) -> Session {
    session_in(Mode::Paper, tweak, prepare, realtime).await
}

/// A live-mode session: orders reach the mock's matching engine under our caller name, so they
/// survive the engine being rebuilt (what a paper broker, rebuilt with the engine, cannot show).
async fn session_live(prepare: impl FnOnce(&Path, &ManualClock)) -> Session {
    session_in(
        Mode::Live,
        |_| {},
        |home, clock| {
            std::fs::create_dir_all(home.join("ops")).expect("mkdir");
            std::fs::write(home.join("ops/GO_LIVE"), "go live\n").expect("arm");
            prepare(home, clock);
        },
        None,
    )
    .await
}

async fn session_in(
    mode: Mode,
    tweak: impl FnOnce(&mut Config),
    prepare: impl FnOnce(&Path, &ManualClock),
    realtime: Option<RealtimeFeed>,
) -> Session {
    let home = tempfile::tempdir().expect("tmp");
    let mut cfg = config();
    tweak(&mut cfg);
    let clock = ManualClock::at("2026-10-08T16:00:00Z".parse().expect("ts"));
    let mock = Arc::new(MockPlatform::new(
        Arc::new(clock.clone()),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
            collateral: true,
        },
    ));
    seed(&mock);
    write_market_map(home.path(), &cfg);
    prepare(home.path(), &clock);
    let mut engine = boot(home.path(), cfg, &clock, &mock, mode, 1, realtime);
    engine.bootstrap().await;
    Session {
        home,
        clock,
        mock,
        engine,
    }
}

/// Build an engine on `home` (no I/O until `bootstrap`). A second call with another pid is what a
/// process restart looks like: nothing in memory, only the intents log and the platform.
fn boot(
    home: &Path,
    cfg: Config,
    clock: &ManualClock,
    mock: &Arc<MockPlatform>,
    mode: Mode,
    pid: u32,
    realtime: Option<RealtimeFeed>,
) -> Engine {
    build(BuildOpts {
        cfg,
        home: home.to_owned(),
        mode,
        clock: Arc::new(clock.clone()),
        platform: mock.clone(),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime,
        needle: None,
        enr: None,
        host: "test".into(),
        pid,
        kill: None,
    })
    .expect("engine")
}

impl Session {
    async fn tick(&mut self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
        self.engine.step().await;
        self.engine.settle().await;
    }

    async fn paper_position(&self, ex: &str) -> i64 {
        let p = self.engine.account().positions(SLUG).await.expect("positions");
        p.positions
            .iter()
            .find(|p| p.exchange_id.as_str() == ex)
            .map_or(0, |p| p.quantity.round() as i64)
    }

    async fn paper_open_orders(&self) -> usize {
        all_orders(
            self.engine.account().as_ref(),
            &TournamentId::new(TID),
            OrderStatusFilter::Open,
            None,
        )
        .await
        .expect("orders")
        .len()
    }

    fn intents(&self, strategy: &str) -> Vec<(String, OrderState)> {
        intents_of(&self.engine, strategy)
    }

    /// Open orders on one exchange as the account reports them (the racer keeps its own orders
    /// resting on exchanges 1-3, so far-quoter tests count per exchange).
    async fn open_orders_on(&self, ex: &str) -> usize {
        all_orders(
            self.engine.account().as_ref(),
            &TournamentId::new(TID),
            OrderStatusFilter::Open,
            Some(&ExchangeId::new(ex)),
        )
        .await
        .expect("orders")
        .len()
    }

    /// The far quoter's resting rungs on `ex`: `(side, price pips, quantity)`.
    fn ladder(&self, ex: &str) -> Vec<(v1::Side, u32, i64)> {
        ladder_of(&self.engine, ex)
    }
}

fn intents_of(engine: &Engine, strategy: &str) -> Vec<(String, OrderState)> {
    engine
        .gateway()
        .oms()
        .lock()
        .all()
        .filter(|t| t.intent.strategy == strategy)
        .map(|t| (t.intent.intent_id.clone(), t.state))
        .collect()
}

fn ladder_of(engine: &Engine, ex: &str) -> Vec<(v1::Side, u32, i64)> {
    let mut rungs: Vec<_> = engine
        .gateway()
        .oms()
        .lock()
        .live()
        .filter(|t| t.intent.strategy == "farquote" && t.intent.exchange_id == ex)
        .map(|t| {
            (
                v1::Side::try_from(t.intent.side).expect("side"),
                t.intent.price_pips,
                t.remaining,
            )
        })
        .collect();
    rungs.sort_unstable();
    rungs
}

/// Cash a ladder ties up (SUSQies).
fn reserved(rungs: &[(v1::Side, u32, i64)]) -> f64 {
    rungs
        .iter()
        .map(|(_, price, qty)| f64::from(*price) / 10_000.0 * *qty as f64)
        .sum()
}

#[tokio::test]
async fn racer_finds_and_trades_the_seeded_violation_on_paper() {
    let mut s = session(|_, _| {}).await;
    s.tick(1).await;

    let racer = s.intents("racer");
    assert_eq!(racer.len(), 3, "one atomic bundle of three NO legs: {racer:?}");
    assert!(
        racer
            .iter()
            .all(|(id, st)| id.starts_with("racer:rel-9:") && *st == OrderState::Filled),
        "{racer:?}"
    );
    let q1 = s.paper_position("1").await;
    assert!(q1 < 0, "bought NO on leg 1");
    assert_eq!(s.paper_position("2").await, q1);
    assert_eq!(s.paper_position("3").await, q1);
    // Paper never reaches the platform, and the live liquidity is untouched.
    assert_eq!(s.mock.engine_orders_created(), 0);
    assert_eq!(s.mock.open_orders_of("alice").len(), 6);

    // The same decision in the same second is a duplicate intent, never a second order.
    s.engine.step().await;
    s.engine.settle().await;
    assert_eq!(s.intents("racer").len(), 3);

    // Status snapshot reflects the session.
    let now = s.clock.now_ns();
    let snap = status::snapshot(&s.engine, now, 1);
    assert_eq!(snap.mode, "paper");
    assert_eq!(snap.trader, "rust-live");
    assert!(!snap.killed);
    assert!(snap.positions.len() >= 3);
    let path = s.home.path().join("state/status.json");
    status::write(&path, &snap).expect("write status");
    let back: v1::StatusSnapshot =
        susq_contracts::from_json(&std::fs::read_to_string(path).expect("read")).expect("parse");
    assert_eq!(back.mode, "paper");
}

#[tokio::test]
async fn kill_file_mid_run_cancels_everything_and_freezes() {
    let mut s = session(|home, clock| write_targets(home, clock.now_ns(), "targets-1.json")).await;
    s.tick(1).await;
    let scenario = s.intents("scenario");
    assert_eq!(scenario.len(), 1, "{scenario:?}");
    assert_eq!(scenario[0].1, OrderState::Acked, "rests at the 0.52 cap");
    assert_eq!(s.paper_open_orders().await, 1);

    std::fs::write(s.home.path().join("ops/KILL"), "drill: kill mid-run").expect("kill");
    s.tick(1).await;
    assert_eq!(s.paper_open_orders().await, 0, "kill cancels everything");
    let before = s.engine.gateway().oms().lock().all().count();
    s.tick(5).await;
    s.tick(5).await;
    assert_eq!(
        s.engine.gateway().oms().lock().all().count(),
        before,
        "nothing new while killed"
    );
    assert!(status::snapshot(&s.engine, s.clock.now_ns(), 2).killed);
}

/// A paper session only competes for the paper lease: another paper process holding it makes
/// this one read-only.
#[tokio::test]
async fn a_lease_held_by_another_paper_process_prevents_any_write() {
    let bridge_clock = Arc::new(Mutex::default());
    let bc = bridge_clock.clone();
    let mut s = session(move |home, clock| {
        write_targets(home, clock.now_ns(), "targets-1.json");
        let bridge = Lease::new(
            LeaseConfig {
                path: home.join("ops/state/trader.paper.lease"),
                ttl: Duration::from_secs(30),
                holder: "paper-peer".into(),
                host: "test".into(),
                pid: 4242,
            },
            Arc::new(clock.clone()),
        );
        assert!(matches!(
            bridge.acquire().expect("bridge lease"),
            LeaseStatus::Held(_)
        ));
        *bc.lock() = Some(bridge);
    })
    .await;
    for _ in 0..4 {
        // The bridge keeps renewing its lease.
        if let Some(b) = bridge_clock.lock().as_ref() {
            b.acquire().expect("renew");
        }
        s.tick(5).await;
    }
    assert_eq!(
        s.engine.gateway().oms().lock().all().count(),
        0,
        "no intent was even written"
    );
    assert_eq!(s.paper_open_orders().await, 0);
    assert_eq!(s.paper_position("1").await, 0);
    let lease_health = s.engine.world.health.get("lease").map(|h| h.state.clone());
    assert_eq!(lease_health.as_deref(), Some("degraded"));
}

/// The live bridge's lease is not ours to take in paper mode: a paper session trades on its own
/// lease and never touches `ops/state/trader.lease`, even once the bridge's lease has lapsed.
#[tokio::test]
async fn paper_mode_never_takes_the_live_lease() {
    let live_path = |home: &Path| home.join("ops/state/trader.lease");
    let mut s = session(move |home, clock| {
        write_targets(home, clock.now_ns(), "targets-1.json");
        let bridge = Lease::new(
            LeaseConfig {
                path: live_path(home),
                ttl: Duration::from_secs(30),
                holder: "bridge".into(),
                host: "test".into(),
                pid: 4242,
            },
            Arc::new(clock.clone()),
        );
        assert!(matches!(
            bridge.acquire().expect("bridge lease"),
            LeaseStatus::Held(_)
        ));
    })
    .await;
    // Well past the bridge's 30 s TTL without a renewal: the live lease has lapsed.
    for _ in 0..10 {
        s.tick(5).await;
    }
    assert!(s.engine.gateway().lease().holds(), "paper lease is ours");
    let lease_health = s.engine.world.health.get("lease").map(|h| h.state.clone());
    assert_eq!(lease_health.as_deref(), Some("ok"));
    assert!(
        s.engine.gateway().oms().lock().all().count() > 0,
        "paper trading proceeds on its own lease"
    );
    let on_disk: v1::TraderLease = susq_contracts::from_json(
        &std::fs::read_to_string(live_path(s.home.path())).expect("live lease file"),
    )
    .expect("lease json");
    assert_eq!(on_disk.holder, "bridge", "the live lease file was left alone");
    assert!(s.home.path().join("ops/state/trader.paper.lease").exists());
}

#[tokio::test]
async fn stale_targets_stop_new_exposure_until_fresh_ones_arrive() {
    let mut s = session(|home, clock| write_targets(home, clock.now_ns(), "targets-1.json")).await;
    s.tick(1).await;
    assert_eq!(
        s.intents("scenario").len(),
        1,
        "fresh targets: first tranche placed"
    );

    // 37 h later the file is past max_age (36 h); the resting tranche has long expired.
    s.tick(37 * 3600).await;
    s.tick(1).await;
    let scenario = s.intents("scenario");
    assert_eq!(scenario.len(), 1, "stale targets: no new exposure: {scenario:?}");
    assert_eq!(scenario[0].1, OrderState::Expired);
    assert!(!s.engine.targets_increase_allowed());
    let targets_health = s.engine.world.health.get("targets").map(|h| h.state.clone());
    assert_eq!(targets_health.as_deref(), Some("degraded"));

    // A fresh file restores it.
    write_targets(s.home.path(), s.clock.now_ns(), "targets-2.json");
    s.tick(1).await;
    assert_eq!(s.intents("scenario").len(), 2, "fresh targets: trading resumes");
}

#[tokio::test]
async fn everything_is_cancelled_at_the_last_order_gate() {
    let mut s = session(|home, clock| write_targets(home, clock.now_ns(), "targets-1.json")).await;
    s.tick(1).await;
    assert_eq!(s.paper_open_orders().await, 1);
    // Jump to 16:54:30Z on Nov 4 with fresh targets: still trading.
    s.clock.set_ns(susq_core::clock::to_ns(
        "2026-11-04T16:54:30Z".parse().expect("ts"),
    ));
    write_targets(s.home.path(), s.clock.now_ns(), "targets-2.json");
    s.tick(1).await;
    assert!(s.paper_open_orders().await >= 1, "resting before the gate");
    s.clock.set_ns(susq_core::clock::to_ns(
        "2026-11-04T16:55:00Z".parse().expect("ts"),
    ));
    s.tick(0).await;
    assert_eq!(s.paper_open_orders().await, 0, "nothing rests into the lock");
    // Expirations are capped at 16:55Z anyway; the explicit cancel-all must still have fired.
    assert!(
        s.engine
            .world
            .events
            .iter()
            .any(|e| e.contains("last-order gate")),
        "{:?}",
        s.engine.world.events
    );
    let before = s.engine.gateway().oms().lock().all().count();
    s.tick(10).await;
    assert_eq!(
        s.engine.gateway().oms().lock().all().count(),
        before,
        "no new orders after the gate"
    );
}

/// `state/fair_live.json` as the research scheduler writes it: header + fair only.
fn write_fair(home: &Path, produced_at_ns: i64, values: &[(&str, f64)]) {
    let tf = TargetFile {
        header: Some(Header {
            schema_version: susq_contracts::SCHEMA_VERSION,
            produced_at_ns,
            producer: Some(Producer {
                name: "python-scheduler".into(),
                git_sha: "test".into(),
            }),
            seq: 1,
        }),
        fair: values
            .iter()
            .map(|(ex, p)| v1::FairValue {
                exchange_id: (*ex).into(),
                p_yes: *p,
                p_lo: *p,
                p_hi: *p,
                source: "0.8*kalshi+0.2*sim".into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let path = home.join("state/fair_live.json");
    std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, susq_contracts::to_json(&tf).expect("json")).expect("write");
    std::fs::rename(tmp, path).expect("rename");
}

#[tokio::test]
async fn fresh_fair_feeds_the_sniper_and_a_stale_feed_stops_it() {
    // House leg: ask 0.55, fair 0.62 → 7 pts through fair, more than the 3.5-pt edge.
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("4", 0.62)])).await;
    s.tick(1).await;
    let sniper = s.intents("sniper");
    assert_eq!(sniper.len(), 1, "{sniper:?}");
    assert_eq!(sniper[0].1, OrderState::Filled);
    assert_eq!(
        s.paper_position("4").await,
        100,
        "took the visible depth at <= 0.58"
    );

    // The feed keeps updating: another opportunity is taken (paper never consumes live depth).
    write_fair(s.home.path(), s.clock.now_ns() + S, &[("4", 0.62)]);
    s.tick(1).await;
    assert_eq!(s.intents("sniper").len(), 2);

    // The scheduler dies: after stale_after_s (60 s) nothing new is built on the old fair.
    s.tick(61).await;
    s.tick(1).await;
    assert_eq!(s.intents("sniper").len(), 2, "stale fair: sniper stops");
    let fair = s.engine.world.health.get("fair").map(|h| h.state.clone());
    assert_eq!(fair.as_deref(), Some("down"));
}

#[tokio::test]
async fn stale_fair_cancels_market_maker_quotes() {
    let mut s = session_with(
        |cfg| {
            cfg.strategies.mm.enabled = true;
            cfg.strategies.sniper.enabled = false;
        },
        |home, clock| write_fair(home, clock.now_ns(), &[("4", 0.525)]),
    )
    .await;
    s.tick(1).await;
    let live = |s: &Session| s.intents("mm").into_iter().filter(|(_, st)| st.is_live()).count();
    assert_eq!(live(&s), 2, "two-sided quotes rest: {:?}", s.intents("mm"));

    s.tick(61).await;
    assert_eq!(
        live(&s),
        0,
        "stale fair cancels every quote: {:?}",
        s.intents("mm")
    );
    s.tick(1).await;
    assert_eq!(live(&s), 0, "and nothing is re-quoted");
}

#[tokio::test]
async fn a_malformed_fair_file_is_refused_and_the_old_values_age_out() {
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("4", 0.62)])).await;
    s.tick(1).await;
    assert_eq!(s.intents("sniper").len(), 1);
    std::fs::write(
        s.home.path().join("state/fair_live.json"),
        "{\"header\": {\"schemaVersion\"",
    )
    .expect("write");
    s.tick(1).await;
    assert!(
        s.engine
            .world
            .events
            .iter()
            .any(|e| e.starts_with("fair file rejected"))
    );
    let fair = s.engine.world.health.get("fair").map(|h| h.state.clone());
    assert_eq!(
        fair.as_deref(),
        Some("degraded"),
        "still inside the freshness window"
    );
    assert_eq!(
        s.intents("sniper").len(),
        2,
        "the last good values are still fresh, so trading continues"
    );
    s.tick(61).await;
    s.tick(1).await;
    assert_eq!(s.intents("sniper").len(), 2, "once they age out, nothing more");
}

/// EXP-003 through the whole engine, live against the mock's matching engine: the account holds a
/// near-certain CA-GOV leg; before the unwind start the sniper leaves it alone, after it the engine
/// sells the holding into the bid (within the 2-pt haircut) and never sells more than it holds.
#[tokio::test]
async fn the_sniper_unwinds_a_near_certain_holding_after_the_start() {
    let tweak = |cfg: &mut Config| {
        // The session clock is Oct 8; start the unwind an hour in.
        cfg.strategies.sniper.unwind_from_utc = Some("2026-10-08T17:00:00Z".parse().expect("ts"));
        // The mock account is almost all cash; ask for all of it so the holding may go.
        cfg.strategies.sniper.unwind_target_cash_frac = 1.0;
        cfg.strategies.farquote.enabled = false;
        cfg.strategies.racer.enabled = false;
    };
    let mut s = session_in(
        Mode::Live,
        tweak,
        |home, clock| {
            std::fs::create_dir_all(home.join("ops")).expect("mkdir");
            std::fs::write(home.join("ops/GO_LIVE"), "go live\n").expect("arm");
            write_fair(home, clock.now_ns(), &[("6", 0.98)]);
        },
        None,
    )
    .await;
    // The account buys bob's 100 YES at 0.99 (the next step's reconcile picks it up).
    s.mock
        .seed_order(CALLER, "6", Side::Yes, Action::Buy, 100, px(9900));
    s.tick(1).await;
    assert_eq!(s.paper_position("6").await, 100);
    assert!(s.intents("sniper").is_empty(), "nothing is sold before the start");

    s.clock.advance(Duration::from_secs(3600));
    write_fair(s.home.path(), s.clock.now_ns(), &[("6", 0.98)]);
    s.tick(1).await;
    let sells = s.intents("sniper");
    assert_eq!(sells.len(), 1, "{sells:?}");
    assert!(sells[0].0.starts_with("sniper:6:unwind:"), "{sells:?}");
    assert_eq!(
        sells[0].1,
        OrderState::Filled,
        "alice's 0.97 bid is within 2 pts of 0.98"
    );
    assert_eq!(
        s.paper_position("6").await,
        0,
        "sold exactly what it held, never short"
    );

    write_fair(s.home.path(), s.clock.now_ns() + S, &[("6", 0.98)]);
    s.tick(31).await;
    assert_eq!(s.intents("sniper").len(), 1, "nothing left to unwind");
}

// ───────────────────────────── far quoter ─────────────────────────────

/// The far quoter's cadence from the shared config, in whole seconds for `tick`.
fn farquote_interval_s() -> u64 {
    config().strategies.farquote.interval_s
}

/// The NO ladder on the near-zero governor leg (Cup 0.01 / 0.03, fair 0.02): four rungs at the
/// configured prices, within the reserve and the per-market cap, left alone by the next pass.
#[tokio::test]
async fn far_quoter_rests_a_no_ladder_on_a_near_zero_market_once() {
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("5", 0.02)])).await;
    let cfg = config().strategies.farquote;
    s.tick(1).await;
    let rungs = s.ladder("5");
    assert_eq!(rungs.len(), cfg.levels.len(), "{rungs:?}");
    assert!(rungs.iter().all(|(side, _, _)| *side == v1::Side::No));
    let prices: Vec<u32> = rungs.iter().map(|r| r.1).collect();
    let want: Vec<u32> = cfg.levels.iter().map(|p| (p * 10_000.0).round() as u32).collect();
    assert_eq!(prices, want);
    assert!(
        s.intents("farquote")
            .iter()
            .all(|(id, st)| id.starts_with("farquote:5:no:") && *st == OrderState::Acked),
        "rest, far from the live book: {:?}",
        s.intents("farquote")
    );
    assert_eq!(s.open_orders_on("5").await, 4);
    assert_eq!(s.paper_position("5").await, 0);
    let equity = s.engine.world.equity;
    let cost = reserved(&rungs);
    assert!(cost <= cfg.reserve_frac * equity + 1e-6);
    assert!(cost <= cfg.max_market_frac * equity + 1e-6);
    assert!(
        cost > 0.99 * cfg.max_market_frac * equity,
        "the per-market budget is used: {cost}"
    );

    // Not due again until the interval has passed, and then the ladder is recognised as resting.
    s.tick(1).await;
    assert_eq!(s.intents("farquote").len(), 4);
    write_fair(
        s.home.path(),
        s.clock.now_ns() + farquote_interval_s() as i64 * S,
        &[("5", 0.02)],
    );
    s.tick(farquote_interval_s()).await;
    assert_eq!(s.intents("farquote").len(), 4, "a second pass adds nothing");
    assert_eq!(s.open_orders_on("5").await, 4);
}

#[tokio::test]
async fn far_quoter_rests_a_yes_ladder_on_a_near_one_market() {
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("6", 0.98)])).await;
    s.tick(1).await;
    let rungs = s.ladder("6");
    assert_eq!(rungs.len(), 4, "{rungs:?}");
    assert!(
        rungs
            .iter()
            .all(|(side, price, _)| *side == v1::Side::Yes && *price <= 1000)
    );
    assert!(
        s.intents("farquote")
            .iter()
            .all(|(_, st)| *st == OrderState::Acked)
    );
    assert!(s.ladder("5").is_empty(), "no fair for the other leg");
}

#[tokio::test]
async fn far_quoter_pulls_the_ladder_when_fair_leaves_the_band() {
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("5", 0.02)])).await;
    s.tick(1).await;
    assert_eq!(s.ladder("5").len(), 4);
    // Out of the band (but still consistent with the 0.02 Cup price, so not a mapping error).
    write_fair(
        s.home.path(),
        s.clock.now_ns() + farquote_interval_s() as i64 * S,
        &[("5", 0.10)],
    );
    s.tick(farquote_interval_s()).await;
    assert!(s.ladder("5").is_empty(), "{:?}", s.intents("farquote"));
    assert_eq!(s.open_orders_on("5").await, 0);
    assert!(
        s.intents("farquote")
            .iter()
            .all(|(_, st)| *st == OrderState::Cancelled),
        "{:?}",
        s.intents("farquote")
    );
}

#[tokio::test]
async fn far_quoter_leaves_ordinary_markets_alone() {
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("4", 0.525)])).await;
    s.tick(1).await;
    assert!(s.intents("farquote").is_empty());
}

#[tokio::test]
async fn a_stale_fair_leaves_resting_ladders_alone() {
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("5", 0.02)])).await;
    s.tick(1).await;
    let before = s.ladder("5");
    assert_eq!(before.len(), 4);
    // The scheduler dies: the next pass finds a fair far older than stale_after_s.
    s.tick(farquote_interval_s()).await;
    assert_eq!(s.ladder("5"), before, "no verdict, so the ladder rests on");
    assert!(
        s.intents("farquote")
            .iter()
            .all(|(_, st)| *st == OrderState::Acked)
    );
}

#[tokio::test]
async fn far_quote_ladders_are_recognised_after_a_restart() {
    let mut s = session_live(|home, clock| write_fair(home, clock.now_ns(), &[("5", 0.02)])).await;
    s.tick(1).await;
    let before = s.ladder("5");
    assert_eq!(before.len(), 4, "{:?}", s.intents("farquote"));
    let on_platform = |s: &Session| {
        s.mock
            .open_orders_of(CALLER)
            .into_iter()
            .filter(|o| o.exchange_id.as_str() == "5")
            .count()
    };
    assert_eq!(on_platform(&s), 4, "resting on the platform");
    // The process goes away without a shutdown (a crash): the lease lapses, the orders stay.
    s.engine.settle().await;
    s.engine.gateway().lease().release().expect("release");

    // A fresh process: nothing in memory, only the intents log and the platform's open orders.
    let mut fresh = boot(s.home.path(), config(), &s.clock, &s.mock, Mode::Live, 2, None);
    fresh.bootstrap().await;
    assert_eq!(
        ladder_of(&fresh, "5"),
        before,
        "restored from the intents log and reconciled"
    );
    write_fair(s.home.path(), s.clock.now_ns() + S, &[("5", 0.02)]);
    s.clock.advance(Duration::from_secs(1));
    fresh.step().await;
    fresh.settle().await;
    assert_eq!(
        ladder_of(&fresh, "5"),
        before,
        "the first pass of the new process adds nothing"
    );
    assert_eq!(on_platform(&s), 4, "no second ladder reached the platform");
    assert_eq!(intents_of(&fresh, "farquote").len(), 4);
}

#[tokio::test]
async fn duplicate_ladders_are_trimmed_to_one_per_level() {
    let mut s = session(|home, clock| write_fair(home, clock.now_ns(), &[("5", 0.02)])).await;
    s.tick(1).await;
    let rungs = s.ladder("5");
    assert_eq!(rungs.len(), 4);
    // Simulate a ladder placed twice (what restarts without strategy attribution did).
    for (_, price, qty) in &rungs {
        let spec = susq_core::OrderSpec {
            intent_id: format!("farquote:5:no:{price}:dup"),
            strategy: "farquote".into(),
            exchange_id: ExchangeId::new("5"),
            market_id: None,
            side: Side::No,
            action: Action::Buy,
            quantity: susq_core::Qty(*qty),
            price: px(*price),
            ttl: Duration::from_secs(3600),
            needle_derived: false,
            fair_yes: None,
        };
        s.engine
            .place(&susq_core::OrderBundle::single(spec))
            .expect("duplicate rung placed");
    }
    s.engine.settle().await;
    assert_eq!(s.ladder("5").len(), 8);
    assert_eq!(s.open_orders_on("5").await, 8);
    write_fair(
        s.home.path(),
        s.clock.now_ns() + farquote_interval_s() as i64 * S,
        &[("5", 0.02)],
    );
    s.tick(farquote_interval_s()).await;
    assert_eq!(
        s.ladder("5"),
        rungs,
        "one rung per level survives, at today's size"
    );
    assert_eq!(s.open_orders_on("5").await, 4);
    let cancelled = s
        .intents("farquote")
        .iter()
        .filter(|(_, st)| *st == OrderState::Cancelled)
        .count();
    assert_eq!(cancelled, 4);
}

/// With the lease held elsewhere the far quoter's pass is skipped outright, and once the lease is
/// ours the next pass is not a burst of catch-up passes.
#[tokio::test]
async fn far_quoter_skips_the_pass_while_the_gate_is_closed() {
    let peer_lease = Mutex::new(None);
    let mut s = session(|home, clock| {
        write_fair(home, clock.now_ns(), &[("5", 0.02)]);
        let peer = Lease::new(
            LeaseConfig {
                path: home.join("ops/state/trader.paper.lease"),
                ttl: Duration::from_secs(30),
                holder: "paper-peer".into(),
                host: "test".into(),
                pid: 4242,
            },
            Arc::new(clock.clone()),
        );
        assert!(matches!(
            peer.acquire().expect("peer lease"),
            LeaseStatus::Held(_)
        ));
        *peer_lease.lock() = Some(peer);
    })
    .await;
    s.tick(1).await;
    assert!(s.intents("farquote").is_empty(), "no lease: not even an intent");
    // The peer lets go; our next renewal takes the lease and the next due pass rests the ladder.
    peer_lease
        .lock()
        .take()
        .expect("peer")
        .release()
        .expect("release");
    write_fair(
        s.home.path(),
        s.clock.now_ns() + farquote_interval_s() as i64 * S,
        &[("5", 0.02)],
    );
    s.tick(farquote_interval_s()).await;
    assert!(s.engine.gateway().lease().holds());
    assert_eq!(s.ladder("5").len(), 4, "{:?}", s.intents("farquote"));
}

/// A versioned `books` item for one exchange, and a batch on that exchange's market channel.
fn rt_batch(ex: &str, market: &str, version: u64, bid: f64, ask: f64) -> RealtimeEvent {
    let level = |price: f64| RtLevel {
        price,
        quantity: 10.0,
    };
    RealtimeEvent::MarketBatch {
        channel: format!("tournament:{TID}:market:{market}"),
        batch: MarketBatch {
            book_dirty: vec![BookDirty {
                exchange_id: ExchangeId::new(ex),
                market_id: Some(MarketId::new(market)),
                tournament_id: Some(TournamentId::new(TID)),
                at: None,
            }],
            books: vec![RtBook {
                exchange_id: ExchangeId::new(ex),
                market_id: Some(MarketId::new(market)),
                tournament_id: Some(TournamentId::new(TID)),
                version: Some(version),
                as_of: None,
                bids: vec![level(bid)],
                asks: vec![level(ask)],
                at: None,
                next_expiry_at: None,
            }],
            ..MarketBatch::default()
        },
    }
}

/// Versioned books in a per-market batch replace the cached book without a REST fetch, older
/// versions are ignored, and a resync scoped to one market's channel leaves other markets alone.
#[tokio::test]
async fn realtime_books_are_applied_by_version_and_market_resyncs_stay_scoped() {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut s = session_full(|_| {}, |_, _| {}, Some(RealtimeFeed::fixed(rx))).await;
    let ex1 = ExchangeId::new("1");
    let ex4 = ExchangeId::new("4");
    let best_bid = |s: &Session, ex: &ExchangeId| {
        s.engine
            .world
            .books
            .get(ex)
            .and_then(|b| b.best_bid())
            .map(|l| l.price.get())
    };
    tx.try_send(rt_batch("1", "m1", 2, 0.11, 0.12)).expect("send");
    tx.try_send(rt_batch("4", "m2", 1, 0.21, 0.22)).expect("send");
    s.tick(1).await;
    assert_eq!(best_bid(&s, &ex1), Some(1100), "the batch's book was applied");
    assert_eq!(best_bid(&s, &ex4), Some(2100));
    assert!(
        !s.engine.world.books.is_dirty(&ex1),
        "a book in the batch clears its own dirty mark"
    );
    // An older version is stale and ignored: its `bookDirty` still stands, so the book comes
    // back from REST (the seeded 0.40 bid), never from the stale item. A newer one is applied.
    tx.try_send(rt_batch("1", "m1", 1, 0.31, 0.32)).expect("send");
    s.tick(1).await;
    assert_eq!(best_bid(&s, &ex1), Some(4000));
    tx.try_send(rt_batch("1", "m1", 3, 0.33, 0.34)).expect("send");
    s.tick(1).await;
    assert_eq!(best_bid(&s, &ex1), Some(3300));
    // A revision gap on m1's channel: m1's books are refetched from REST, m2's is untouched.
    tx.try_send(RealtimeEvent::Resync {
        reason: ResyncReason::RevisionGap {
            channel: format!("tournament:{TID}:market:m1"),
            expected: 3,
            got: Some(1),
        },
    })
    .expect("send");
    s.tick(1).await;
    assert_eq!(
        best_bid(&s, &ex1),
        Some(4000),
        "refetched from the seeded REST book"
    );
    assert_eq!(best_bid(&s, &ex4), Some(2100));
    assert!(!s.engine.world.books.is_dirty(&ex4));
    // A reconnect taints every book, including markets nobody is working.
    tx.try_send(RealtimeEvent::Resync {
        reason: ResyncReason::Reconnect,
    })
    .expect("send");
    s.tick(1).await;
    assert!(s.engine.world.books.is_dirty(&ex4));
}

// ── Election night: a real needle state machine on the same watch channel production uses ──────

/// The needle as the engine sees it: the pure monitor state, published on a watch channel the way
/// `NeedleMonitor` publishes it. Messages carry the session clock so liveness is judged for real,
/// and each subject numbers its own messages, as `susq-needle` does (the watchdog checks gaps).
struct FakeNeedle {
    state: susq_bus::NeedleState,
    tx: tokio::sync::watch::Sender<susq_bus::NeedleSnapshot>,
    heartbeat_seq: u64,
    posterior_seq: u64,
}

impl FakeNeedle {
    fn new(cfg: &Config, now: i64) -> (Self, tokio::sync::watch::Receiver<susq_bus::NeedleSnapshot>) {
        let races = [
            "2026-SEN-OH",
            "2026-GOV-TX",
            "2026-GOV-CA",
            "2026-GOV-NV",
            "2026-HOUSE-CA-22",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let state = susq_bus::NeedleState::new(susq_bus::WatchdogCfg::from_config(&cfg.needle), races)
            .with_posterior_ttl(Duration::from_secs(cfg.needle.posterior_max_age_s));
        let (tx, rx) = tokio::sync::watch::channel(state.snapshot(now));
        (
            Self {
                state,
                tx,
                heartbeat_seq: 0,
                posterior_seq: 0,
            },
            rx,
        )
    }

    fn header(seq: u64, now: i64) -> Option<Header> {
        Some(Header {
            schema_version: susq_contracts::SCHEMA_VERSION,
            produced_at_ns: now,
            producer: Some(Producer {
                name: "needle".into(),
                git_sha: "test".into(),
            }),
            seq,
        })
    }

    fn publish(&mut self, now: i64) {
        self.state.on_tick(now);
        let _ = self.tx.send(self.state.snapshot(now));
    }

    fn heartbeat(&mut self, now: i64) {
        self.heartbeat_seq += 1;
        let header = Self::header(self.heartbeat_seq, now);
        self.state.on_message(
            susq_bus::NeedleMessage::Heartbeat(v1::Heartbeat {
                header,
                status: "ok".into(),
                last_input_seq: 0,
            }),
            now,
        );
        self.publish(now);
    }

    /// One posterior: Democrat wins `race` with `p_dem`, the margin it rests on, `n_batches` batches,
    /// 60% of the expected vote counted.
    fn posterior(
        &mut self,
        now: i64,
        race: &str,
        p_dem: f64,
        margin_mean: f64,
        margin_sd: f64,
        n_batches: u32,
    ) {
        self.posterior_counted(now, race, p_dem, margin_mean, margin_sd, n_batches, 0.6);
    }

    /// [`Self::posterior`] with `reported_frac` of the expected vote counted.
    #[allow(clippy::too_many_arguments)]
    fn posterior_counted(
        &mut self,
        now: i64,
        race: &str,
        p_dem: f64,
        margin_mean: f64,
        margin_sd: f64,
        n_batches: u32,
        reported_frac: f64,
    ) {
        self.posterior_seq += 1;
        let header = Self::header(self.posterior_seq, now);
        self.state.on_message(
            susq_bus::NeedleMessage::Posteriors(v1::RacePosteriors {
                header,
                races: vec![v1::RacePosterior {
                    race_id: race.into(),
                    p_dem,
                    p_rep: 1.0 - p_dem,
                    margin_mean,
                    margin_sd,
                    n_batches,
                    reported_frac,
                    ..Default::default()
                }],
            }),
            now,
        );
        self.publish(now);
    }
}

/// Readiness audit step I. The election-night executor turns a bus posterior into a real order on
/// the exchange, goes quiet the moment the needle's heartbeat stops (even with fresh edge on the
/// book), and comes back only after the watchdog's resume streak and a fresh posterior.
#[tokio::test]
async fn needle_posterior_drives_an_election_night_buy_and_silence_stops_it() {
    let home = tempfile::tempdir().expect("tmp");
    let mut cfg = config();
    cfg.strategies.en.enabled = true;
    let clock = ManualClock::at("2026-10-08T16:00:00Z".parse().expect("ts"));
    let mock = Arc::new(MockPlatform::new(
        Arc::new(clock.clone()),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
            collateral: true,
        },
    ));
    seed(&mock);
    write_market_map(home.path(), &cfg);
    let (mut needle, rx) = FakeNeedle::new(&cfg, clock.now_ns());
    let mut engine = build(BuildOpts {
        cfg: cfg.clone(),
        home: home.path().to_owned(),
        mode: Mode::Paper,
        clock: Arc::new(clock.clone()),
        platform: mock.clone(),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime: None,
        needle: Some(rx),
        enr: None,
        host: "test".into(),
        pid: 1,
        kill: None,
    })
    .expect("engine");
    engine.bootstrap().await;
    let mut s = Session {
        home,
        clock,
        mock,
        engine,
    };

    // Healthy needle, Texas governor counted 60% with the Democrat 5 ± 3 ahead (q = 0.95) on three
    // county batches. The Cup still asks 0.03 for the Democrat: the trade the executor exists for.
    for _ in 0..cfg.needle.resume_after_healthy {
        needle.heartbeat(s.clock.now_ns());
    }
    needle.posterior(s.clock.now_ns(), "2026-GOV-TX", 0.95, 5.0, 3.0, 3);
    s.tick(1).await;
    s.tick(1).await;
    let en: Vec<_> = s.intents("en");
    assert!(
        !en.is_empty(),
        "a posterior with 90 points of edge must produce an election-night buy"
    );
    assert!(
        s.paper_position("5").await > 0,
        "the buy filled against the 0.03 ask on the Texas Democrat"
    );
    let placed = en.len();

    // The needle goes silent for longer than heartbeat_timeout_s. Fresh size appears at 0.03.
    // Nothing needle-derived may be sent: the posterior is no longer trusted.
    s.mock
        .seed_order("carol", "5", Side::Yes, Action::Sell, 100, px(300));
    s.clock
        .advance(Duration::from_secs(cfg.needle.heartbeat_timeout_s + 2));
    needle.publish(s.clock.now_ns());
    s.tick(1).await;
    s.tick(1).await;
    assert_eq!(s.intents("en").len(), placed, "a silent needle places nothing");
    assert!(
        s.intents("en").iter().all(|(_, state)| !state.is_live()),
        "every needle-derived order still working when the needle fell silent was cancelled"
    );
    let health = status::snapshot(&s.engine, s.clock.now_ns(), 1).health;
    let item = health
        .iter()
        .find(|h| h.name == "needle")
        .expect("needle health item");
    assert_ne!(item.state, "ok", "the dashboard shows the needle down");

    // One heartbeat is not enough to trust it again (resume_after_healthy).
    needle.heartbeat(s.clock.now_ns());
    s.tick(1).await;
    assert_eq!(
        s.intents("en").len(),
        placed,
        "one heartbeat does not resume trading"
    );

    // The full resume streak plus a fresh posterior: trading resumes on the new size.
    for _ in 0..cfg.needle.resume_after_healthy {
        needle.heartbeat(s.clock.now_ns());
    }
    needle.posterior(s.clock.now_ns(), "2026-GOV-TX", 0.95, 5.0, 3.0, 4);
    s.tick(1).await;
    s.tick(1).await;
    assert!(
        s.intents("en").len() > placed,
        "a healthy needle with edge trades again"
    );
}

/// One lock-time-rule scenario: a race the needle projects for the Democrat on a partial count, its
/// Democrat leg on the Cup, and what the needle said before going quiet.
struct LockCase {
    race: &'static str,
    market: &'static str,
    title: &'static str,
    ex: &'static str,
    cluster: &'static str,
    p_dem: f64,
    margin_mean: f64,
    margin_sd: f64,
    n_batches: u32,
    reported_frac: f64,
}

/// EXP-006 lock-time rule, end to end. Nevada governor at 60% counted on Wednesday morning: the
/// Democrat trails on the print, the needle projects D +3 ± 1.2 (q 0.994) from the late Clark mail
/// still to come, and then goes quiet on the race (Clark posts once a day) while it keeps
/// heartbeating. Two hours later the Cup sells the Democrat at 0.40 off the print. With the lock
/// rule on the executor buys the Democrat on that quiet posterior; with it off nothing is sent,
/// because the posterior is older than `[needle] posterior_max_age_s`.
const NEVADA: LockCase = LockCase {
    race: "2026-GOV-NV",
    market: "m5",
    title: "NV-GOV",
    ex: "7",
    cluster: "state:NV",
    p_dem: 0.994,
    margin_mean: 3.0,
    margin_sd: 1.2,
    n_batches: 40,
    reported_frac: 0.6,
};

/// The same rule on a California House seat, which the needle can only model because the CA SoS
/// feed reports the district as ONE unit (`CA-HD22`, from its districtwide row). 55% counted at the
/// lock with the Democrat trailing on the print (the 2024 CA-13 and CA-45 pattern); the needle
/// projects D +2.5 ± 1.0 from the uncounted 45%, the count-order override for `2026-HOUSE-CA-*`
/// registers −3 ± 1.5 at the lock (the count still to come favours the Democrat), and the Cup
/// prices the Democrat off the print.
const CA_22: LockCase = LockCase {
    race: "2026-HOUSE-CA-22",
    market: "m6",
    title: "CA-22",
    ex: "8",
    cluster: "state:CA",
    p_dem: 0.99,
    margin_mean: 2.5,
    margin_sd: 1.0,
    n_batches: 12,
    reported_frac: 0.55,
};

async fn nevada_lock_session(lock_rule_enabled: bool) -> (Session, usize) {
    lock_session(&NEVADA, lock_rule_enabled).await
}

async fn lock_session(case: &LockCase, lock_rule_enabled: bool) -> (Session, usize) {
    let home = tempfile::tempdir().expect("tmp");
    let mut cfg = config();
    cfg.strategies.en.enabled = true;
    cfg.strategies.en.lock_rule_enabled = lock_rule_enabled;
    let clock = ManualClock::at("2026-11-04T12:30:00Z".parse().expect("ts"));
    let mock = Arc::new(MockPlatform::new(
        Arc::new(clock.clone()),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
            collateral: true,
        },
    ));
    seed(&mock);
    mock.add_market(case.market, case.title, &[(case.ex, "DEM")]);
    // While the needle is fresh nobody sells the Democrat below 0.995: no edge. (The bid sits low so
    // the later 0.40 offer rests instead of crossing it.)
    mock.seed_order("alice", case.ex, Side::Yes, Action::Buy, 100, px(3500));
    mock.seed_order("bob", case.ex, Side::Yes, Action::Sell, 100, px(9950));
    write_market_map_plus(
        home.path(),
        &cfg,
        vec![map_rule(case.ex, case.market, case.race, "DEM", &[case.cluster])],
    );
    let (mut needle, rx) = FakeNeedle::new(&cfg, clock.now_ns());
    let engine = build(BuildOpts {
        cfg: cfg.clone(),
        home: home.path().to_owned(),
        mode: Mode::Paper,
        clock: Arc::new(clock.clone()),
        platform: mock.clone(),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime: None,
        needle: Some(rx),
        enr: None,
        host: "test".into(),
        pid: 1,
        kill: None,
    })
    .expect("engine (the count-order table loads)");
    let mut s = Session {
        home,
        clock,
        mock,
        engine,
    };
    s.engine.bootstrap().await;

    for _ in 0..cfg.needle.resume_after_healthy {
        needle.heartbeat(s.clock.now_ns());
    }
    needle.posterior_counted(
        s.clock.now_ns(),
        case.race,
        case.p_dem,
        case.margin_mean,
        case.margin_sd,
        case.n_batches,
        case.reported_frac,
    );
    s.tick(1).await;
    s.tick(1).await;
    assert!(
        s.intents("en").is_empty(),
        "{}: no edge while the Cup asks 0.995 for the Democrat",
        case.race
    );

    // Two hours on, heartbeating every 5 s (well inside heartbeat_timeout_s), no new batch for
    // the race; the engine steps every half hour.
    for _ in 0..4 {
        for _ in 0..(30 * 60 / 5) {
            s.clock.advance(Duration::from_secs(5));
            needle.heartbeat(s.clock.now_ns());
        }
        s.tick(1).await;
    }
    assert!(
        s.intents("en").is_empty(),
        "still nothing while nobody offers the Democrat cheaply"
    );
    // 14:30Z: the Cup now sells the Democrat at 0.40 off the print.
    s.mock
        .seed_order("carol", case.ex, Side::Yes, Action::Sell, 100, px(4000));
    // Without realtime the book is refetched by REST once it is half its max age old.
    for _ in 0..cfg.live.book_max_age_s {
        needle.heartbeat(s.clock.now_ns());
        s.tick(1).await;
    }
    let n = s.engine.world.needle.as_ref().expect("needle view");
    assert!(
        n.healthy && n.posteriors.is_empty(),
        "healthy needle, nothing fresh"
    );
    if lock_rule_enabled {
        assert!(
            n.quiet_posteriors.contains_key(case.race),
            "the quiet posterior is passed on"
        );
    } else {
        assert!(
            n.quiet_posteriors.is_empty(),
            "nothing quiet is passed with the rule off"
        );
    }
    let placed = s.intents("en").len();
    (s, placed)
}

#[tokio::test]
async fn lock_rule_buys_a_trailing_nevada_democrat_on_a_quiet_needle_only_when_on() {
    let (s, placed) = nevada_lock_session(true).await;
    assert!(
        placed > 0,
        "the lock rule must buy the Democrat on the quiet posterior"
    );
    assert!(
        s.paper_position("7").await > 0,
        "the buy filled against the 0.40 ask"
    );
    let (s, placed) = nevada_lock_session(false).await;
    assert_eq!(
        placed, 0,
        "with the lock rule off a quiet posterior is never traded"
    );
    assert_eq!(s.paper_position("7").await, 0);
}

#[tokio::test]
async fn lock_rule_buys_a_trailing_california_house_democrat_only_when_on() {
    let (s, placed) = lock_session(&CA_22, true).await;
    assert!(
        placed > 0,
        "the lock rule must buy the CA-22 Democrat on the quiet posterior"
    );
    assert!(
        s.paper_position(CA_22.ex).await > 0,
        "the buy filled against the 0.40 ask"
    );
    let (s, placed) = lock_session(&CA_22, false).await;
    assert_eq!(
        placed, 0,
        "with the lock rule off a quiet House posterior is never traded"
    );
    assert_eq!(s.paper_position(CA_22.ex).await, 0);
}

// ── Throttled order-book reads must not stall the loop ───────────────────────────────────────────

/// Order-book reads that wait `delay` of real time first: what the book throttle (about 15 reads a
/// minute, slower after a 429) does to a step that wants eight books.
struct SlowBooks {
    inner: Arc<MockPlatform>,
    delay: Duration,
}

#[async_trait::async_trait]
impl PlatformAdapter for SlowBooks {
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
        tokio::time::sleep(self.delay).await;
        self.inner.orderbook(tid, exchange, depth).await
    }
    async fn place_order(&self, req: &PlaceOrderRequest) -> PlatformResult<OrderAck> {
        self.inner.place_order(req).await
    }
    async fn place_batch(&self, req: &BatchRequest) -> PlatformResult<BatchResponse> {
        self.inner.place_batch(req).await
    }
    async fn place_multi_leg(&self, req: &MultiLegRequest) -> PlatformResult<MultiLegResponse> {
        self.inner.place_multi_leg(req).await
    }
    async fn cancel_order(&self, id: susq_core::OrderId) -> PlatformResult<CancelOutcome> {
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

/// Oct 9-11: steps waiting on eight throttled book reads ran past the 120 s health limit and the
/// watchdog restarted the paper trader eleven times. A step now spends at most the book budget on
/// book reads, and the books it skipped arrive over the next steps.
#[tokio::test(start_paused = true)]
async fn throttled_book_reads_cannot_hold_a_step_past_the_budget() {
    let home = tempfile::tempdir().expect("tmp");
    let cfg = config();
    let budget = Duration::from_millis(cfg.live.book_refresh_budget_ms);
    let clock = ManualClock::at("2026-10-08T16:00:00Z".parse().expect("ts"));
    let mock = Arc::new(MockPlatform::new(
        Arc::new(clock.clone()),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
            collateral: true,
        },
    ));
    seed(&mock);
    write_market_map(home.path(), &cfg);
    let mut engine = build(BuildOpts {
        cfg,
        home: home.path().to_owned(),
        mode: Mode::Paper,
        clock: Arc::new(clock.clone()),
        platform: Arc::new(SlowBooks {
            inner: mock.clone(),
            delay: Duration::from_secs(2),
        }),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime: None,
        needle: None,
        enr: None,
        host: "test".into(),
        pid: 1,
        kill: None,
    })
    .expect("engine");
    engine.bootstrap().await;

    // Eight books at 2 s each would hold one step for 16 s; each step stops at the budget and the
    // books still arrive over the next steps, so the racer trades within a few of them.
    let mut steps = 0;
    while intents_of(&engine, "racer").is_empty() && steps < 6 {
        clock.advance(Duration::from_secs(1));
        let started = tokio::time::Instant::now();
        engine.step().await;
        let took = started.elapsed();
        assert!(
            took <= budget + Duration::from_secs(1),
            "a step spent {took:?} on book reads, budget {budget:?}"
        );
        engine.settle().await;
        steps += 1;
    }
    assert!(steps > 1, "the books took more than one step to arrive");
    assert_eq!(
        intents_of(&engine, "racer").len(),
        3,
        "the racer traded once its books arrived"
    );
}

/// Oct 14-20: every step re-read the book of each exchange with a resting paper order (26 of them
/// by the afternoon) one throttled read at a time, so a step took a minute or more and, when paper
/// writes queued on the same throttle, past the 120 s health limit: the watchdog restarted the
/// paper trader every 25-35 minutes. The re-read now gets the book budget too, stalest first, and
/// the skipped books catch up over the next steps.
#[tokio::test(start_paused = true)]
async fn resting_paper_orders_cannot_hold_a_step_past_the_budget() {
    let home = tempfile::tempdir().expect("tmp");
    let mut cfg = config();
    // Orders placed straight on the paper account are unknown to the OMS: keep reconcile from
    // cancelling them as orphans while the steps run.
    cfg.oms.reconcile_interval_s = 3600;
    let budget = Duration::from_millis(cfg.live.book_refresh_budget_ms);
    let book_max_age_ns = i64::try_from(cfg.live.book_max_age_s).expect("small") * S;
    let clock = ManualClock::at("2026-10-08T16:00:00Z".parse().expect("ts"));
    let mock = Arc::new(MockPlatform::new(
        Arc::new(clock.clone()),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
            collateral: true,
        },
    ));
    seed(&mock);
    write_market_map(home.path(), &cfg);
    let mut engine = build(BuildOpts {
        cfg,
        home: home.path().to_owned(),
        mode: Mode::Paper,
        clock: Arc::new(clock.clone()),
        platform: Arc::new(SlowBooks {
            inner: mock.clone(),
            delay: Duration::from_secs(2),
        }),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime: None,
        needle: None,
        enr: None,
        host: "test".into(),
        pid: 1,
        kill: None,
    })
    .expect("engine");
    engine.bootstrap().await;

    // A resting paper bid on all six exchanges, well under the live bids.
    let tid = TournamentId::new(TID);
    for ex in ["1", "2", "3", "4", "5", "6"] {
        let req = PlaceOrderRequest {
            order: susq_platform::OrderRequest::limit(
                ExchangeId::new(ex),
                Side::Yes,
                Action::Buy,
                susq_core::Qty(1),
                px(100),
                clock.now() + chrono::Duration::hours(6),
                tid.clone(),
            ),
            idempotency_key: format!("rest-{ex}"),
        };
        let ack = engine.account().place_order(&req).await.expect("paper order");
        assert!(ack.open, "the bid on {ex} rests");
    }
    let paper = engine.paper().expect("paper mode").clone();
    clock.advance(Duration::from_secs(
        u64::try_from(book_max_age_ns / S).expect("pos") + 1,
    ));
    assert_eq!(
        paper.resting_due(book_max_age_ns).len(),
        6,
        "every resting book is stale"
    );

    // Six books at 2 s each would hold a step for 12 s on top of the decision books; each step
    // stops at the budget and the resting books are all re-read within a few steps.
    let mut steps = 0;
    while !paper.resting_due(book_max_age_ns).is_empty() && steps < 6 {
        clock.advance(Duration::from_secs(1));
        let started = tokio::time::Instant::now();
        engine.step().await;
        let took = started.elapsed();
        assert!(
            took <= 2 * budget + Duration::from_secs(1),
            "a step spent {took:?} on book reads, budget {budget:?} twice"
        );
        engine.settle().await;
        steps += 1;
    }
    assert!(steps > 1, "the resting books took more than one step");
    assert!(
        paper.resting_due(book_max_age_ns).is_empty(),
        "every resting book was re-read within six steps"
    );
}

// ── EXP-014: contender mode ──────────────────────────────────────────────────────────────────

/// A paper session with the election-night executor on, tweaked by `tweak`, fed by a fake needle.
/// The racer is off so nothing else trades the Ohio legs.
async fn needle_session(tweak: impl FnOnce(&mut Config)) -> (Session, FakeNeedle, Config) {
    let home = tempfile::tempdir().expect("tmp");
    let mut cfg = config();
    cfg.strategies.en.enabled = true;
    cfg.strategies.racer.enabled = false;
    tweak(&mut cfg);
    let clock = ManualClock::at("2026-10-08T16:00:00Z".parse().expect("ts"));
    let mock = Arc::new(MockPlatform::new(
        Arc::new(clock.clone()),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
            collateral: true,
        },
    ));
    seed(&mock);
    write_market_map(home.path(), &cfg);
    let (needle, rx) = FakeNeedle::new(&cfg, clock.now_ns());
    let mut engine = build(BuildOpts {
        cfg: cfg.clone(),
        home: home.path().to_owned(),
        mode: Mode::Paper,
        clock: Arc::new(clock.clone()),
        platform: mock.clone(),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime: None,
        needle: Some(rx),
        enr: None,
        host: "test".into(),
        pid: 1,
        kill: None,
    })
    .expect("engine");
    engine.bootstrap().await;
    let s = Session {
        home,
        clock,
        mock,
        engine,
    };
    (s, needle, cfg)
}

/// Two steps 6 s apart (past half of book_max_age_s, so the books are re-read) with the needle's
/// heartbeat kept alive.
async fn ticks_alive(s: &mut Session, needle: &mut FakeNeedle) {
    for _ in 0..2 {
        needle.heartbeat(s.clock.now_ns());
        s.tick(6).await;
    }
}

fn intents_on(s: &Session, strategy: &str, ex: &str) -> Vec<(String, OrderState)> {
    s.intents(strategy)
        .into_iter()
        .filter(|(id, _)| id.starts_with(&format!("{strategy}:{ex}:")))
        .collect()
}

/// EXP-014 end to end: with contender mode on, the executor buys the Texas Democrat, holds it
/// through a bid that the default profile would have rolled at, sells it once the needle calls the
/// race, and only then has the budget to buy the next race (Ohio), which the night budget refused
/// while Texas was held. When the needle falls silent nothing is sent despite fresh edge.
#[tokio::test]
async fn contender_mode_holds_to_the_call_chains_into_the_next_race_and_stops_when_the_needle_is_down() {
    // A budget of 150 SUSQies (0.15% of 100k) holds one of the two buys below, not both.
    let (mut s, mut needle, cfg) = needle_session(|cfg| {
        cfg.strategies.en.contender_mode = true;
        cfg.strategies.en.contender_budget_frac = 0.0015;
    })
    .await;
    for _ in 0..cfg.needle.resume_after_healthy {
        needle.heartbeat(s.clock.now_ns());
    }

    // 1. Texas: D +5 ± 3 (q 0.95) on three batches; the Cup asks 0.03 for 100 shares.
    needle.posterior(s.clock.now_ns(), "2026-GOV-TX", 0.95, 5.0, 3.0, 3);
    ticks_alive(&mut s, &mut needle).await;
    assert_eq!(s.paper_position("5").await, 100, "bought the Texas Democrat");

    // 2. Ohio: D +4 ± 2 (q 0.977), the Cup asks 0.45: a 50-point edge, but Texas holds the budget.
    needle.posterior(s.clock.now_ns(), "2026-SEN-OH", 0.977, 4.0, 2.0, 40);
    ticks_alive(&mut s, &mut needle).await;
    assert!(
        intents_on(&s, "en", "1").is_empty(),
        "the night budget refuses the second race while the first is held"
    );

    // 3. Texas tightens to q 0.93 and a 0.92 bid appears: the default profile rolls here (within
    //    2 points); contender mode holds, the race is not called (SE 0.045) and 0.92 < 0.93.
    //    (The 0.03 ask our paper buy left on the mock is taken first so the bid does not cross it.)
    s.mock
        .seed_order("dave", "5", Side::Yes, Action::Buy, 100, px(300));
    s.mock
        .seed_order("alice", "5", Side::Yes, Action::Buy, 100, px(9200));
    needle.posterior(s.clock.now_ns(), "2026-GOV-TX", 0.93, 4.4, 3.0, 5);
    ticks_alive(&mut s, &mut needle).await;
    assert_eq!(s.paper_position("5").await, 100, "held to the call");
    assert!(
        intents_on(&s, "en", "5")
            .iter()
            .all(|(id, _)| !id.contains(":roll:"))
    );

    // 4. The needle calls Texas (D +12 ± 3, SE 4.5e-5): sold at no less than q − 8 pts = 0.92.
    needle.posterior(s.clock.now_ns(), "2026-GOV-TX", 0.9999, 12.0, 3.0, 8);
    ticks_alive(&mut s, &mut needle).await;
    assert_eq!(s.paper_position("5").await, 0, "sold once the race was called");
    assert!(
        intents_on(&s, "en", "5")
            .iter()
            .any(|(id, _)| id.contains(":roll:"))
    );

    // 5. The proceeds free the budget: the chain moves on to Ohio.
    needle.posterior(s.clock.now_ns(), "2026-SEN-OH", 0.977, 4.0, 2.0, 41);
    ticks_alive(&mut s, &mut needle).await;
    assert!(!intents_on(&s, "en", "1").is_empty(), "chained into Ohio");
    assert!(s.paper_position("1").await > 0, "the Ohio buy filled");
    let placed = s.intents("en").len();

    // 6. The needle goes silent past heartbeat_timeout_s while fresh size sits at 0.45: nothing
    //    needle-derived is sent and nothing is left working.
    s.mock
        .seed_order("carol", "1", Side::Yes, Action::Sell, 100, px(4500));
    s.clock
        .advance(Duration::from_secs(cfg.needle.heartbeat_timeout_s + 2));
    needle.publish(s.clock.now_ns());
    s.tick(1).await;
    s.tick(1).await;
    assert_eq!(s.intents("en").len(), placed, "a silent needle places nothing");
    assert!(s.intents("en").iter().all(|(_, state)| !state.is_live()));
}

/// EXP-014 risk wiring: needle-derived orders on a deep book (100k+ within 3c of the touch, read off
/// the engine's fresh books) build the market past 5% only with contender mode on.
#[tokio::test]
async fn contender_cap_reads_depth_off_the_fresh_books() {
    async fn accepted(contender: bool, deep: bool) -> usize {
        let (mut s, _needle, _) = needle_session(|cfg| {
            cfg.strategies.en.enabled = false;
            cfg.strategies.en.contender_mode = contender;
        })
        .await;
        if deep {
            // 300,000 at 0.45: 135k of depth within 3c of the touch.
            s.mock
                .seed_order("carol", "1", Side::Yes, Action::Sell, 300_000, px(4500));
        }
        s.tick(1).await; // load the books
        let mut ok = 0;
        for i in 0..6 {
            let spec = susq_core::OrderSpec {
                intent_id: format!("en:1:yes:deep{i}"),
                strategy: "en".into(),
                exchange_id: ExchangeId::new("1"),
                market_id: Some(MarketId::new("m1")),
                side: Side::Yes,
                action: Action::Buy,
                quantity: susq_core::Qty(4_000), // 1,800 SUSQies at 0.45, under the 2% order cap
                price: px(4500),
                ttl: Duration::from_secs(20),
                needle_derived: true,
                fair_yes: None,
            };
            if s.engine.place(&susq_core::OrderBundle::single(spec)).is_ok() {
                ok += 1;
            }
            s.engine.settle().await;
        }
        ok
    }
    let off = accepted(false, true).await;
    let on = accepted(true, true).await;
    let thin = accepted(true, false).await;
    assert!(off < 6, "off: the 5% market cap binds ({off} accepted)");
    assert_eq!(on, 6, "on with a deep book: the market builds past 5%");
    assert_eq!(thin, off, "on with a thin book: the 5% cap still binds");
}
