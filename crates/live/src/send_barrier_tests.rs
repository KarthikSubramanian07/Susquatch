//! Deterministically leave an acknowledged fill uncollected, as when it arrives during refresh.
use super::*;
use susq_core::{Action, Config, ManualClock, Mode, Pips, Side, TournamentId};
use susq_oms::FillDelta;
use susq_platform::mock::{MockConfig, MockPlatform};

async fn fixture() -> (Engine, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("home");
    let mut cfg = Config::from_toml_str(include_str!("../../../config/susq.toml")).expect("config");
    cfg.realtime.enabled = false;
    cfg.strategies.racer.enabled = false;
    cfg.strategies.sniper.enabled = false;
    cfg.strategies.mm.enabled = false;
    cfg.strategies.scenario.enabled = false;
    cfg.strategies.farquote.enabled = false;
    cfg.strategies.en.enabled = true;
    cfg.strategies.en.contender_mode = true;
    cfg.strategies.en.count_order_enabled = false;
    cfg.strategies.en.require_corroboration = false;
    cfg.strategies.en.count_order_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/count_order.toml");
    let clock = Arc::new(ManualClock::at("2026-11-04T02:00:00Z".parse().expect("time")));
    let mock = Arc::new(MockPlatform::new(
        clock.clone(),
        MockConfig {
            tournament_id: "test".into(),
            starting_balance: 100_000.0,
            ..Default::default()
        },
    ));
    let mut e = crate::wiring::build(crate::wiring::BuildOpts {
        cfg,
        home: home.path().to_owned(),
        mode: Mode::Paper,
        clock,
        platform: mock,
        journal: susq_journal::Journal::discard(),
        tournament: TournamentId::new("test"),
        paper_balance: 100_000.0,
        realtime: None,
        needle: None,
        enr: None,
        host: "test".into(),
        pid: 1,
        kill: None,
    })
    .expect("engine");
    e.d.gateway.lease().acquire().expect("lease");
    e.world.equity = 100_000.0;
    e.world.initial_equity = 100_000.0;
    e.world.cash = 100_000.0;
    e.world.markets = susq_core::MarketIndex::new(&v1::MarketMap {
        markets: vec![v1::MarketRule {
            exchange_id: "1076".into(),
            market_id: "m".into(),
            race_id: "2026-SEN-OH".into(),
            kind: "race_party".into(),
            party: "DEM".into(),
            verified: true,
            ..Default::default()
        }],
        ..Default::default()
    });
    e.world.needle = Some(susq_strategies::NeedleView {
        healthy: true,
        epoch: 1,
        posteriors: HashMap::from([(
            "2026-SEN-OH".into(),
            v1::RacePosterior {
                race_id: "2026-SEN-OH".into(),
                p_dem: 0.95,
                p_rep: 0.05,
                n_batches: 100,
                ..Default::default()
            },
        )]),
        ..Default::default()
    });
    e.world.books.apply_snapshot(susq_book::L2Book::from_levels(
        ExchangeId::new("1076"),
        vec![susq_book::Level {
            price: Pips::new(9300).expect("price"),
            qty: 10_000,
        }],
        vec![susq_book::Level {
            price: Pips::new(9600).expect("price"),
            qty: 10_000,
        }],
        e.d.clock.now_ns(),
    ));
    (e, home)
}

/// OMS acknowledges the real order, but deliberately retain its completed JoinHandle in sends.
async fn filled(e: &mut Engine, action: Action, quantity: i64) {
    let now = e.d.clock.now_ns();
    let bundle = OrderBundle::single(susq_core::OrderSpec {
        intent_id: "en:1076:first".into(),
        strategy: "en".into(),
        exchange_id: ExchangeId::new("1076"),
        market_id: None,
        side: Side::Yes,
        action,
        quantity: susq_core::Qty(quantity),
        price: Pips::new(9300).expect("price"),
        ttl: std::time::Duration::from_secs(20),
        needle_derived: false,
        fair_yes: None,
    });
    // Prepare through OMS; emulate the gateway's ack-before-task-collection ordering.
    e.d.gateway
        .oms()
        .lock()
        .prepare(&bundle, e.d.clock.now(), &e.d.cfg.calendar, &e.world.tid)
        .expect("prepare");
    assert!(e.d.gateway.oms().lock().mark_sent(&["en:1076:first".into()], now));
    let ack: susq_platform::OrderAck = serde_json::from_value(serde_json::json!({
        "orderId": 1, "exchangeId": "1076", "open": false,
        "quantityTraded": quantity, "remainingQuantity": 0
    }))
    .expect("ack");
    let delta =
        e.d.gateway
            .oms()
            .lock()
            .apply_ack("en:1076:first", &ack, now)
            .expect("fill");
    let h = tokio::spawn(async move { SendOutcome::Acked(vec![delta]) });
    while !h.is_finished() {
        tokio::task::yield_now().await;
    }
    e.sends.push(h);
    assert_eq!(e.d.gateway.oms().lock().live().count(), 0);
}

#[tokio::test]
async fn filled_sell_with_stale_holding_cannot_sell_again() {
    let (mut e, _home) = fixture().await;
    let ex = ExchangeId::new("1076");
    e.world.positions.apply(&FillDelta {
        exchange_id: ex.clone(),
        yes_delta: 784,
    });
    filled(&mut e, Action::Sell, 784).await;
    let now = e.d.clock.now_ns();
    e.decide_and_act(now).await;
    assert_eq!(
        e.d.gateway.oms().lock().all().count(),
        1,
        "no duplicate sell against stale 784"
    );
    e.collect_sends(now).await;
    assert_eq!(e.world.positions.get(&ex), 0);
    e.decide_and_act(now).await;
    assert_eq!(e.d.gateway.oms().lock().all().count(), 1, "flat after collection");
    // Positive control: the same fresh book/posterior really does trigger an exit when held.
    e.world.positions.apply(&FillDelta {
        exchange_id: ex,
        yes_delta: 784,
    });
    e.decide_and_act(now + S).await;
    assert_eq!(
        e.d.gateway.oms().lock().all().count(),
        2,
        "fixture must trigger contender exit"
    );
    e.settle().await;
}

#[tokio::test]
async fn rest_waits_for_uncollected_buy_and_sell_deltas() {
    for action in [Action::Buy, Action::Sell] {
        let (mut e, _home) = fixture().await;
        filled(&mut e, action, 784).await;
        let now = e.d.clock.now_ns();
        let before = e.world.positions.refreshed_at_ns();
        e.reconcile(now).await;
        assert_eq!(
            e.world.positions.refreshed_at_ns(),
            before,
            "REST must not include an uncollected fill"
        );
        e.collect_sends(now).await;
        assert_eq!(
            e.world.positions.get(&ExchangeId::new("1076")),
            if action == Action::Buy { 784 } else { -784 }
        );
        e.reconcile(now).await;
        assert_eq!(
            e.world.positions.refreshed_at_ns(),
            Some(now),
            "REST resumes after collection"
        );
    }
}

#[tokio::test]
async fn filled_buy_cannot_reuse_stale_position_budget() {
    let (mut e, _home) = fixture().await;
    let now = e.d.clock.now_ns();
    let ex = ExchangeId::new("1076");
    e.world.books.apply_snapshot(susq_book::L2Book::from_levels(
        ex.clone(),
        vec![susq_book::Level {
            price: Pips::new(4000).expect("price"),
            qty: 10_000,
        }],
        vec![susq_book::Level {
            price: Pips::new(4500).expect("price"),
            qty: 10_000,
        }],
        now,
    ));
    filled(&mut e, Action::Buy, 784).await;
    e.decide_and_act(now).await;
    assert_eq!(
        e.d.gateway.oms().lock().all().count(),
        1,
        "no buy using stale exposure"
    );
    e.collect_sends(now).await;
    assert_eq!(e.world.positions.get(&ex), 784, "next risk check sees the buy");
    // Positive control proves this snapshot would otherwise buy.
    e.world.positions = susq_oms::PositionTracker::new();
    e.decide_and_act(now + S).await;
    assert!(
        e.d.gateway.oms().lock().all().count() > 1,
        "fixture must trigger a buy"
    );
    e.settle().await;
}
