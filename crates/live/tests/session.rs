//! A simulated session of the full live loop in paper mode against the mock platform:
//! the racer finds and trades a seeded violation, a kill file mid-run cancels everything,
//! a lease held by the bridge prevents any write, and stale targets stop new exposure.

use std::{path::Path, sync::Arc, time::Duration};

use parking_lot::Mutex;
use serde_json::json;
use susq_contracts::v1::{
    self, Header, MarketMap, MarketRule, Producer, TargetFile, TargetPointer, TargetPosition,
};
use susq_core::{Action, Clock, Config, ManualClock, Mode, Pips, Side, TournamentId};
use susq_gateway::{Lease, LeaseConfig, LeaseStatus};
use susq_journal::Journal;
use susq_live::{
    engine::Engine,
    status,
    wiring::{BuildOpts, build},
};
use susq_oms::OrderState;
use susq_platform::{
    OrderStatusFilter, all_orders,
    mock::{MockConfig, MockPlatform},
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
    let home = tempfile::tempdir().expect("tmp");
    let cfg = config();
    let clock = ManualClock::at("2026-10-08T16:00:00Z".parse().expect("ts"));
    let mock = Arc::new(MockPlatform::new(
        Arc::new(clock.clone()),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
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
        realtime: None,
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

#[tokio::test]
async fn a_lease_held_by_the_bridge_prevents_any_write() {
    let bridge_clock = Arc::new(Mutex::default());
    let bc = bridge_clock.clone();
    let mut s = session(move |home, clock| {
        write_targets(home, clock.now_ns(), "targets-1.json");
        let bridge = Lease::new(
            LeaseConfig {
                path: home.join("ops/state/trader.lease"),
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
