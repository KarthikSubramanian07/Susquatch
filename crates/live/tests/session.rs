//! A simulated session of the full live loop in paper mode against the mock platform:
//! the racer finds and trades a seeded violation, a kill file mid-run cancels everything,
//! a lease held by the bridge prevents any write, and stale targets stop new exposure.

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
    OrderStatusFilter, all_orders,
    mock::{MockConfig, MockPlatform},
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
    cfg
}

/// A seeded exchange: three mutually exclusive legs whose YES bids sum to 1.05, plus a House leg.
fn seed(mock: &MockPlatform) {
    mock.add_market(
        "m1",
        "Who wins OH-SEN",
        &[("1", "DEM"), ("2", "REP"), ("3", "IND")],
    );
    mock.add_market("m2", "House control", &[("4", "DEM")]);
    for (ex, bid, ask) in [
        ("1", 4000, 4500),
        ("2", 3500, 4000),
        ("3", 3000, 3500),
        ("4", 5000, 5500),
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

fn write_market_map(home: &Path, cfg: &Config) {
    let rule = |ex: &str, market: &str, race: &str, party: &str, clusters: &[&str]| MarketRule {
        market_id: market.into(),
        exchange_id: ex.into(),
        title: format!("{race} {party}"),
        race_id: race.into(),
        kind: "race_party".into(),
        party: party.into(),
        clusters: clusters.iter().map(|c| (*c).to_owned()).collect(),
        verified: true,
        ..Default::default()
    };
    let map = MarketMap {
        header: None,
        markets: vec![
            rule("1", "m1", "2026-SEN-OH", "DEM", &["national", "state:OH"]),
            rule("2", "m1", "2026-SEN-OH", "REP", &["national", "state:OH"]),
            rule("3", "m1", "2026-SEN-OH", "IND", &["national", "state:OH"]),
            rule("4", "m2", "", "DEM", &["house"]),
        ],
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
    let mut engine = build(BuildOpts {
        cfg,
        home: home.path().to_owned(),
        mode: Mode::Paper,
        clock: Arc::new(clock.clone()),
        platform: mock.clone(),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime,
        needle: None,
        host: "test".into(),
        pid: 1,
        kill: None,
    })
    .expect("engine");
    engine.bootstrap().await;
    Session {
        home,
        clock,
        mock,
        engine,
    }
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
        self.engine
            .gateway()
            .oms()
            .lock()
            .all()
            .filter(|t| t.intent.strategy == strategy)
            .map(|t| (t.intent.intent_id.clone(), t.state))
            .collect()
    }
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
    assert_eq!(s.mock.open_orders_of("alice").len(), 4);

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
