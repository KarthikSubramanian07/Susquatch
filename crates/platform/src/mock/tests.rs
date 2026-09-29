//! Behavioural tests of the mock engine against the documented platform semantics.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use proptest::prelude::*;
use susq_core::{Action, BookSide, ExchangeId, ManualClock, OrderId, Pips, Side, TournamentId};

use super::*;
use crate::{
    adapter::{PlatformAdapter, all_markets, all_orders},
    error::{PlatformError, RetryClass},
};

const TID: &str = "tid-1";
const SLUG: &str = "midterm-elections";

fn t0() -> DateTime<Utc> {
    "2026-10-01T16:00:00Z".parse().expect("timestamp")
}

fn px(raw: u32) -> Pips {
    Pips::new(raw).expect("valid price")
}

fn tid() -> TournamentId {
    TournamentId::new(TID)
}

fn config(page_size: usize) -> MockConfig {
    MockConfig {
        tournament_id: TID.to_owned(),
        page_size,
        ..MockConfig::default()
    }
}

fn mock_with(cfg: MockConfig) -> (ManualClock, MockPlatform) {
    let clock = ManualClock::at(t0());
    let mock = MockPlatform::new(Arc::new(clock.clone()), cfg);
    mock.add_market(
        "m1",
        "Senate control",
        &[("1", "Democrats"), ("2", "Republicans")],
    );
    mock.add_market("m2", "House control", &[("3", "Democrats")]);
    (clock, mock)
}

fn mock() -> (ManualClock, MockPlatform) {
    mock_with(config(200))
}

fn order(exchange: &str, side: Side, action: Action, quantity: i64, price: u32) -> OrderRequest {
    OrderRequest {
        exchange_id: ExchangeId::new(exchange),
        side,
        action,
        quantity,
        price: px(price),
        expiration_date: t0() + chrono::Duration::hours(1),
        tournament_id: tid(),
    }
}

fn placing(key: &str, order: OrderRequest) -> PlaceOrderRequest {
    PlaceOrderRequest {
        order,
        idempotency_key: key.to_owned(),
    }
}

fn api(err: &PlatformError) -> (u16, &str) {
    (err.status().unwrap_or_default(), err.code().unwrap_or_default())
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[tokio::test]
async fn price_time_priority_trades_at_resting_prices() {
    let (_, m) = mock();
    m.seed_order("a", "1", Side::Yes, Action::Sell, 5, px(6000));
    m.seed_order("b", "1", Side::Yes, Action::Sell, 5, px(5500));
    m.seed_order("c", "1", Side::Yes, Action::Sell, 5, px(5500));

    let ack = m
        .place_order(&placing("k", order("1", Side::Yes, Action::Buy, 12, 6000)))
        .await
        .expect("placed");

    assert!(!ack.open);
    assert_close(ack.quantity_traded, 12.0);
    assert_close(ack.total_cost, 5.0 * 0.55 + 5.0 * 0.55 + 2.0 * 0.60);
    assert_eq!(
        (
            m.position_of("b", "1"),
            m.position_of("c", "1"),
            m.position_of("a", "1")
        ),
        (-5, -5, -2)
    );
    assert_eq!(m.position_of(CALLER, "1"), 12);
    let rest = m.open_orders_of("a");
    assert_eq!(rest.len(), 1);
    assert_close(rest[0].quantity, 3.0);
    assert_close(m.cash_of(CALLER), 1_000.0 - 6.7);
}

#[tokio::test]
async fn partial_fill_rests_the_remainder() {
    let (_, m) = mock();
    m.seed_order("a", "1", Side::Yes, Action::Sell, 4, px(5000));

    let ack = m
        .place_order(&placing("k", order("1", Side::Yes, Action::Buy, 10, 5000)))
        .await
        .expect("placed");

    assert!(ack.open);
    assert_close(ack.quantity_traded, 4.0);
    assert_eq!(ack.remaining_quantity, Some(6.0));
    let open = m
        .orders_page(&tid(), OrderStatusFilter::Open, None, None)
        .await
        .expect("orders");
    assert_eq!(open.data.len(), 1);
    assert_close(open.data[0].quantity, 6.0);
    let book = m
        .orderbook(&tid(), &ExchangeId::new("1"), 10)
        .await
        .expect("book");
    assert_eq!(
        book.bids,
        vec![BookLevel {
            price: 0.5,
            quantity: 6.0
        }]
    );
    assert!(book.asks.is_empty());
}

#[tokio::test]
async fn flat_sell_is_canonicalized_to_the_complement_buy() {
    let (_, m) = mock();
    let ack = m
        .place_order(&placing("k", order("1", Side::Yes, Action::Sell, 10, 3000)))
        .await
        .expect("placed");

    assert_eq!(
        (ack.action, ack.side, ack.quantity),
        (Some(Action::Buy), Some(Side::No), Some(10))
    );
    assert_eq!(ack.price, Some(0.7));
    let book = m
        .orderbook(&tid(), &ExchangeId::new("1"), 10)
        .await
        .expect("book");
    assert_eq!(
        book.asks,
        vec![BookLevel {
            price: 0.3,
            quantity: 10.0
        }]
    );
}

#[tokio::test]
async fn backed_sell_stays_a_sell_and_oversized_sell_is_canonicalized() {
    let (_, m) = mock();
    m.seed_order("a", "1", Side::Yes, Action::Sell, 5, px(4000));
    m.place_order(&placing("buy", order("1", Side::Yes, Action::Buy, 5, 4000)))
        .await
        .expect("bought");

    let backed = m
        .place_order(&placing("s1", order("1", Side::Yes, Action::Sell, 5, 4500)))
        .await
        .expect("sold");
    assert_eq!(
        (backed.action, backed.side),
        (Some(Action::Sell), Some(Side::Yes))
    );

    // The 5 held shares are already offered, so another sell is uncovered.
    let uncovered = m
        .place_order(&placing("s2", order("1", Side::Yes, Action::Sell, 3, 4500)))
        .await
        .expect("sold");
    assert_eq!(
        (uncovered.action, uncovered.side),
        (Some(Action::Buy), Some(Side::No))
    );
    assert_eq!(uncovered.price, Some(0.55));
}

#[tokio::test]
async fn self_trade_prevention_cancels_the_crossing_remainder() {
    let (_, m) = mock();
    let own_bid = m
        .place_order(&placing("bid", order("1", Side::Yes, Action::Buy, 5, 5000)))
        .await
        .expect("bid");
    m.seed_order("a", "1", Side::Yes, Action::Buy, 3, px(5500));

    let ack = m
        .place_order(&placing("ask", order("1", Side::Yes, Action::Sell, 10, 4500)))
        .await
        .expect("ask");

    assert_close(ack.quantity_traded, 3.0);
    assert!(!ack.open);
    assert_eq!(ack.terminal_reason_code.as_deref(), Some("SelfTradePrevented"));
    assert_eq!((m.position_of("a", "1"), m.position_of(CALLER, "1")), (3, -3));
    let open = m.open_orders_of(CALLER);
    assert_eq!(open.len(), 1);
    assert_eq!(Some(open[0].id), own_bid.order_id);
    assert_close(open[0].quantity, 5.0);
}

#[tokio::test]
async fn same_key_replays_and_different_payload_conflicts() {
    let (_, m) = mock();
    let req = placing("k1", order("1", Side::Yes, Action::Buy, 5, 4000));
    let first = m.place_order(&req).await.expect("first");
    let second = m.place_order(&req).await.expect("replay");

    assert_eq!(first, second);
    assert_eq!(m.engine_orders_created(), 1);
    assert_eq!(m.open_orders_of(CALLER).len(), 1);

    let changed = placing("k1", order("1", Side::Yes, Action::Buy, 6, 4000));
    let err = m.place_order(&changed).await.expect_err("conflict");
    assert_eq!(api(&err), (409, "CONFLICT"));
    assert_eq!(err.retry_class(), RetryClass::Terminal);
}

#[tokio::test]
async fn unknown_but_placed_then_retry_places_exactly_one_order() {
    let (_, m) = mock();
    m.inject(Endpoint::PlaceOrder, Fault::UnknownButPlaced);
    let req = placing("k", order("1", Side::Yes, Action::Buy, 5, 4000));

    let err = m.place_order(&req).await.expect_err("unknown outcome");
    assert_eq!(api(&err), (502, "ORDER_STATUS_UNKNOWN"));
    assert!(matches!(err.retry_class(), RetryClass::Retry { .. }));
    assert_eq!(m.engine_orders_created(), 1);

    let ack = m.place_order(&req).await.expect("replayed");
    assert_eq!(m.engine_orders_created(), 1);
    assert_eq!(ack.order_id, Some(m.open_orders_of(CALLER)[0].id));
    assert_eq!(m.calls(Endpoint::PlaceOrder), 2);
}

#[tokio::test]
async fn pre_execution_faults_execute_nothing() {
    let (_, m) = mock();
    let req = placing("k", order("1", Side::Yes, Action::Buy, 5, 4000));
    m.inject(
        Endpoint::PlaceOrder,
        Fault::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        },
    );
    m.inject(Endpoint::PlaceOrder, Fault::Unavailable);
    m.inject(Endpoint::PlaceOrder, Fault::InFlight);
    m.inject(Endpoint::PlaceOrder, Fault::Internal);
    m.inject(Endpoint::PlaceOrder, Fault::Network { executed: false });

    let rate_limited = m.place_order(&req).await.expect_err("429");
    assert_eq!(api(&rate_limited), (429, "RATE_LIMITED"));
    assert_eq!(rate_limited.retry_after(), Some(Duration::from_secs(3)));
    for expected in [
        (503, "SERVICE_UNAVAILABLE"),
        (409, "REQUEST_IN_FLIGHT"),
        (500, "INTERNAL_ERROR"),
    ] {
        assert_eq!(api(&m.place_order(&req).await.expect_err("fault")), expected);
    }
    assert!(matches!(
        m.place_order(&req).await,
        Err(PlatformError::Network { .. })
    ));
    assert_eq!(m.engine_orders_created(), 0);

    m.place_order(&req).await.expect("placed");
    assert_eq!((m.engine_orders_created(), m.calls(Endpoint::PlaceOrder)), (1, 6));
}

#[tokio::test]
async fn network_failure_after_execution_is_replayed_on_retry() {
    let (_, m) = mock();
    m.inject(Endpoint::PlaceOrder, Fault::Network { executed: true });
    let req = placing("k", order("1", Side::Yes, Action::Buy, 5, 4000));

    assert!(matches!(
        m.place_order(&req).await,
        Err(PlatformError::Network { .. })
    ));
    m.place_order(&req).await.expect("replayed");
    assert_eq!(m.engine_orders_created(), 1);
}

#[tokio::test]
async fn orders_expire_lazily() {
    let (clock, m) = mock();
    let mut short = order("1", Side::Yes, Action::Buy, 5, 4000);
    short.expiration_date = t0() + chrono::Duration::seconds(30);
    let ack = m.place_order(&placing("k", short)).await.expect("placed");

    clock.advance(Duration::from_secs(31));
    let open = m
        .orders_page(&tid(), OrderStatusFilter::Open, None, None)
        .await
        .expect("orders");
    assert!(open.data.is_empty());
    let expired = m
        .orders_page(&tid(), OrderStatusFilter::Expired, None, None)
        .await
        .expect("orders");
    assert_eq!(expired.data.len(), 1);
    let id = OrderId(ack.order_id.expect("id"));
    assert_eq!(m.cancel_order(id).await, Ok(CancelOutcome::AlreadyClosed));

    let mut stale = order("1", Side::Yes, Action::Buy, 5, 4000);
    stale.expiration_date = clock.now();
    let err = m.place_order(&placing("k2", stale)).await.expect_err("stale");
    assert_eq!(api(&err), (400, "VALIDATION_ERROR"));
}

#[tokio::test]
async fn multi_leg_rolls_back_every_leg_on_failure() {
    let (_, m) = mock();
    m.seed_order("a", "1", Side::Yes, Action::Sell, 5, px(4000));
    let req = MultiLegRequest {
        legs: vec![
            order("1", Side::Yes, Action::Buy, 5, 4000),
            order("99", Side::Yes, Action::Buy, 1, 5000),
        ],
        idempotency_key: "ml".to_owned(),
        relationship_constraint: None,
    };

    let err = m.place_multi_leg(&req).await.expect_err("leg 1 fails");
    assert_eq!(api(&err), (404, "NOT_FOUND"));
    let PlatformError::Api(e) = &err else {
        panic!("expected an api error, got {err:?}");
    };
    assert_eq!(e.details["leg"], 1);
    assert_eq!(m.position_of(CALLER, "1"), 0);
    assert_close(m.cash_of(CALLER), 1_000.0);
    assert_close(m.open_orders_of("a")[0].quantity, 5.0);
    assert_eq!(m.engine_orders_created(), 0);

    let ok = MultiLegRequest {
        legs: vec![
            order("1", Side::Yes, Action::Buy, 5, 4000),
            order("3", Side::No, Action::Buy, 2, 5000),
        ],
        idempotency_key: "ml2".to_owned(),
        relationship_constraint: None,
    };
    let resp = m.place_multi_leg(&ok).await.expect("placed");
    assert_eq!(resp.results.len(), 2);
    assert_eq!(m.position_of(CALLER, "1"), 5);
    assert_eq!(m.engine_orders_created(), 2);
}

#[tokio::test]
async fn multi_leg_rejects_duplicate_exchanges() {
    let (_, m) = mock();
    let req = MultiLegRequest {
        legs: vec![
            order("1", Side::Yes, Action::Buy, 1, 4000),
            order("1", Side::No, Action::Buy, 1, 4000),
        ],
        idempotency_key: "dup".to_owned(),
        relationship_constraint: None,
    };
    let err = m.place_multi_leg(&req).await.expect_err("duplicate");
    assert_eq!(api(&err), (400, "VALIDATION_ERROR"));
}

#[tokio::test]
async fn batch_reports_mixed_results_and_replays() {
    let (_, m) = mock();
    let req = BatchRequest {
        idempotency_key: "b".to_owned(),
        orders: vec![
            order("1", Side::Yes, Action::Buy, 1, 4000),
            order("99", Side::Yes, Action::Buy, 1, 4000),
        ],
    };

    let resp = m.place_batch(&req).await.expect("batch");
    assert_eq!(resp.http_status, 207);
    assert!(resp.results[0].ok && resp.results[0].ack().is_some());
    assert_eq!(resp.results[1].status, 404);
    assert_eq!(resp.results[1].error_code().as_deref(), Some("NOT_FOUND"));

    assert_eq!(m.place_batch(&req).await.expect("replay"), resp);
    assert_eq!(m.engine_orders_created(), 1);

    let all_bad = BatchRequest {
        idempotency_key: "b2".to_owned(),
        orders: vec![order("99", Side::Yes, Action::Buy, 1, 4000)],
    };
    assert_eq!(m.place_batch(&all_bad).await.expect("batch").http_status, 422);

    let too_big = BatchRequest {
        idempotency_key: "b3".to_owned(),
        orders: vec![order("1", Side::Yes, Action::Buy, 1, 4000); MAX_BATCH + 1],
    };
    assert_eq!(
        api(&m.place_batch(&too_big).await.expect_err("too big")),
        (400, "VALIDATION_ERROR")
    );
}

#[tokio::test]
async fn cancel_semantics() {
    let (_, m) = mock();
    let err = m.cancel_order(OrderId(9_999)).await.expect_err("unknown");
    assert_eq!(api(&err), (404, "NOT_FOUND"));

    let ack = m
        .place_order(&placing("k", order("1", Side::Yes, Action::Buy, 5, 4000)))
        .await
        .expect("placed");
    let id = OrderId(ack.order_id.expect("id"));
    assert_eq!(m.cancel_order(id).await, Ok(CancelOutcome::Cancelled));
    assert_eq!(m.cancel_order(id).await, Ok(CancelOutcome::AlreadyClosed));

    let theirs = m.seed_order("a", "1", Side::Yes, Action::Buy, 1, px(1000));
    assert_eq!(
        api(&m.cancel_order(theirs).await.expect_err("not ours")),
        (403, "FORBIDDEN")
    );
}

#[tokio::test]
async fn cancel_all_respects_scope() {
    let (_, m) = mock();
    for (key, exchange) in [("k1", "1"), ("k2", "2"), ("k3", "3")] {
        m.place_order(&placing(key, order(exchange, Side::Yes, Action::Buy, 1, 1000)))
            .await
            .expect("placed");
    }

    let by_exchange = CancelScope {
        exchange_id: Some(ExchangeId::new("1")),
        market_id: None,
    };
    let resp = m.cancel_all(&tid(), &by_exchange).await.expect("cancel");
    assert_eq!((resp.http_status, resp.cancelled), (200, 1));

    let both = CancelScope {
        exchange_id: Some(ExchangeId::new("1")),
        market_id: Some(MarketId::new("m1")),
    };
    assert_eq!(
        api(&m.cancel_all(&tid(), &both).await.expect_err("both")),
        (400, "VALIDATION_ERROR")
    );

    let by_market = CancelScope {
        exchange_id: None,
        market_id: Some(MarketId::new("m2")),
    };
    assert_eq!(
        m.cancel_all(&tid(), &by_market).await.expect("cancel").cancelled,
        1
    );
    assert_eq!(m.open_orders_of(CALLER).len(), 1);
    assert_eq!(
        m.cancel_all(&tid(), &CancelScope::default())
            .await
            .expect("cancel")
            .cancelled,
        1
    );
}

#[tokio::test]
async fn opposite_buy_nets_and_pays_one_per_pair() {
    let (_, m) = mock();
    m.seed_order("a", "1", Side::Yes, Action::Sell, 10, px(4000));
    m.place_order(&placing("yes", order("1", Side::Yes, Action::Buy, 10, 4000)))
        .await
        .expect("bought yes");
    assert_close(m.cash_of(CALLER), 996.0);

    let positions = m.positions(SLUG).await.expect("positions");
    assert_eq!(positions.positions.len(), 1);
    let yes = &positions.positions[0];
    assert_eq!((yes.quantity, yes.current_price), (10.0, Some(0.4)));
    assert_close(yes.avg_cost, 0.4);
    assert_close(
        m.pnl(SLUG, PnlPeriod::All)
            .await
            .expect("pnl")
            .total_account_value,
        1_000.0,
    );

    m.seed_order("b", "1", Side::Yes, Action::Buy, 10, px(4500));
    let ack = m
        .place_order(&placing("no", order("1", Side::No, Action::Buy, 10, 5500)))
        .await
        .expect("bought no");
    assert_close(ack.total_cost, 5.5);

    // Paid 4.00 for YES and 5.50 for NO; the 10 pairs pay 10.
    assert_eq!(m.position_of(CALLER, "1"), 0);
    assert_close(m.cash_of(CALLER), 1_000.5);
    assert!(m.positions(SLUG).await.expect("positions").positions.is_empty());
    let pnl = m.pnl(SLUG, PnlPeriod::All).await.expect("pnl");
    assert_close(pnl.total_account_value, 1_000.5);
    let tournament = m.tournament(SLUG).await.expect("tournament");
    assert_close(tournament.my_balance, 1_000.5);

    let fills = m.fills_page(SLUG, None).await.expect("fills");
    let signed: Vec<(f64, Side)> = fills.data.iter().map(|f| (f.quantity, f.side)).collect();
    assert_eq!(signed, vec![(10.0, Side::Yes), (-10.0, Side::No)]);
}

#[tokio::test]
async fn insufficient_balance_is_rejected_and_not_remembered() {
    let (_, m) = mock();
    let too_big = placing("k", order("1", Side::Yes, Action::Buy, 3_000, 5000));
    let err = m.place_order(&too_big).await.expect_err("too big");
    assert_eq!(api(&err), (400, "INSUFFICIENT_BALANCE"));

    // Nothing was stored under the key, so a corrected order may reuse it.
    m.place_order(&placing("k", order("1", Side::Yes, Action::Buy, 10, 5000)))
        .await
        .expect("placed");
    assert_eq!(m.engine_orders_created(), 1);
}

#[tokio::test]
async fn request_validation() {
    let (_, m) = mock();
    let mut foreign = order("1", Side::Yes, Action::Buy, 1, 4000);
    foreign.tournament_id = TournamentId::new("other");
    assert_eq!(
        api(&m.place_order(&placing("a", foreign)).await.expect_err("403")),
        (403, "FORBIDDEN")
    );
    for quantity in [0, i64::from(i32::MAX) + 1] {
        let bad = order("1", Side::Yes, Action::Buy, quantity, 4000);
        assert_eq!(
            api(&m.place_order(&placing("b", bad)).await.expect_err("400")),
            (400, "VALIDATION_ERROR")
        );
    }
    let unknown = order("99", Side::Yes, Action::Buy, 1, 4000);
    assert_eq!(
        api(&m.place_order(&placing("c", unknown)).await.expect_err("404")),
        (404, "NOT_FOUND")
    );
    assert_eq!(m.engine_orders_created(), 0);
}

#[tokio::test]
async fn prices_report_top_of_book_and_missing_ids() {
    let (_, m) = mock();
    m.seed_order("a", "1", Side::Yes, Action::Buy, 3, px(4000));
    m.seed_order("b", "1", Side::No, Action::Buy, 3, px(5000));
    let ids = [ExchangeId::new("99"), ExchangeId::new("1")];

    let prices = m.prices(&tid(), &ids).await.expect("prices");
    assert_eq!(prices.missing_ids, vec![ExchangeId::new("99")]);
    let quote = &prices.data[0];
    assert_eq!(
        (quote.best_bid, quote.best_ask, quote.latest_price),
        (Some(0.4), Some(0.5), None)
    );

    let too_many: Vec<ExchangeId> = (0..=MAX_PRICE_IDS)
        .map(|i| ExchangeId::new(i.to_string()))
        .collect();
    assert!(matches!(
        m.prices(&tid(), &too_many).await,
        Err(PlatformError::Invalid(_))
    ));
}

#[tokio::test]
async fn listings_paginate_with_a_small_page_size() {
    let (_, m) = mock_with(config(2));
    m.add_market("m3", "Governors", &[("4", "Democrats")]);
    for i in 0..5_u32 {
        let o = order("1", Side::Yes, Action::Buy, 1, 1000 + 50 * i);
        m.place_order(&placing(&format!("k{i}"), o))
            .await
            .expect("placed");
    }

    let first = m
        .orders_page(&tid(), OrderStatusFilter::Open, None, None)
        .await
        .expect("page");
    assert_eq!(first.data.len(), 2);
    assert!(first.pagination.has_more);
    let all = all_orders(&m, &tid(), OrderStatusFilter::Open, None)
        .await
        .expect("all");
    let ids: Vec<i64> = all.iter().map(|o| o.id).collect();
    assert_eq!(ids.len(), 5);
    assert!(ids.windows(2).all(|w| w[0] < w[1]));

    let markets = all_markets(&m, SLUG, MarketStatusFilter::Open)
        .await
        .expect("markets");
    assert_eq!(markets.len(), 3);
    assert!(
        all_markets(&m, SLUG, MarketStatusFilter::Settled)
            .await
            .expect("markets")
            .is_empty()
    );

    let bad = m
        .orders_page(&tid(), OrderStatusFilter::Open, None, Some("x"))
        .await;
    assert_eq!(api(&bad.expect_err("bad cursor")), (400, "VALIDATION_ERROR"));
}

#[tokio::test]
async fn seeded_reads_are_served_and_filtered() {
    let (_, m) = mock();
    let eval = |id: &str, status: &str, amount: f64| ConstraintEval {
        relationship_id: id.to_owned(),
        kind: RelationshipType::MutuallyExclusive,
        violation_amount: amount,
        reason: String::new(),
        suggested_corrective_trades: Vec::new(),
        observed_prices: None,
        bound: None,
        lower_bound: None,
        upper_bound: None,
        current_value: None,
        evaluation_status: status.to_owned(),
        diagnostic: None,
        tournament_id: None,
        direction: None,
        computed_at: None,
    };
    m.set_constraints(vec![
        eval("r1", "violated", 0.05),
        eval("r2", "violated", 0.001),
        eval("r3", "satisfied", 0.0),
    ]);
    let violations = m.constraints(&tid(), true, 0.01).await.expect("constraints");
    assert_eq!(violations.data.len(), 1);
    assert_eq!(violations.violations_count, 1);
    assert_eq!(
        m.constraints(&tid(), false, 0.0).await.expect("all").data.len(),
        3
    );

    m.set_realtime_token(RealtimeToken {
        token: "jwt".to_owned(),
        ..RealtimeToken::default()
    });
    assert_eq!(m.realtime_token().await.expect("token").token, "jwt");
    let tournament = m.tournament(SLUG).await.expect("tournament");
    assert_eq!((tournament.id.as_str(), tournament.market_count), (TID, 2));
    assert_eq!(
        api(&m.tournament("nope").await.expect_err("404")),
        (404, "NOT_FOUND")
    );
}

const OWNERS: [&str; 3] = [CALLER, "a", "b"];
const EXCHANGES: [&str; 2] = ["1", "2"];

fn seeded_op() -> impl Strategy<Value = (usize, usize, Side, Action, i64, u32)> {
    (
        0..OWNERS.len(),
        0..EXCHANGES.len(),
        prop_oneof![Just(Side::Yes), Just(Side::No)],
        prop_oneof![Just(Action::Buy), Just(Action::Sell)],
        1..20_i64,
        1..=199_u32,
    )
}

proptest! {
    #[test]
    fn matching_conserves_shares_and_cash_and_never_crosses(ops in prop::collection::vec(seeded_op(), 1..60)) {
        let (_, m) = mock();
        for (owner, exchange, side, action, qty, ticks) in ops {
            m.seed_order(OWNERS[owner], EXCHANGES[exchange], side, action, qty, px(ticks * 50));

            let mut long_yes = 0;
            for ex in EXCHANGES {
                let positions: Vec<i64> = OWNERS.iter().map(|o| m.position_of(o, ex)).collect();
                prop_assert_eq!(positions.iter().sum::<i64>(), 0);
                long_yes += positions.iter().map(|q| q.max(&0)).sum::<i64>();

                let state = &m.inner.lock().state;
                let exchange = ExchangeId::new(ex);
                if let (Some(bid), Some(ask)) = (state.best(&exchange, BookSide::Bid), state.best(&exchange, BookSide::Ask)) {
                    prop_assert!(bid < ask, "crossed book on {}: {:?} >= {:?}", ex, bid, ask);
                }
            }
            // Every outstanding YES/NO pair was paid for with exactly 1 unit of cash.
            let cash_spent: f64 = 1_000.0 - OWNERS.iter().map(|o| m.cash_of(o)).sum::<f64>();
            prop_assert!((cash_spent - long_yes as f64).abs() < 1e-6, "cash {} vs pairs {}", cash_spent, long_yes);
        }
    }
}

#[tokio::test]
async fn replacing_liquidity_fills_resting_orders_it_crosses() {
    let (_, m) = mock();
    let ex = ExchangeId::new("1");
    m.replace_liquidity("market", &ex, &[(px(4000), 10)], &[(px(6000), 10)]);
    let ack = m
        .place_order(&placing("k", order("1", Side::Yes, Action::Buy, 5, 5000)))
        .await
        .expect("rests");
    assert!(ack.open);
    // The live book moves: someone now offers at 0.49, through our 0.50 bid.
    let placed = m.replace_liquidity("market", &ex, &[(px(4000), 10), (px(3000), 0)], &[(px(4900), 3)]);
    assert_eq!(placed, 2, "zero-size levels are skipped");
    assert_eq!(m.position_of(CALLER, "1"), 3, "filled at our resting price");
    assert_eq!(m.open_orders_of(CALLER)[0].quantity, 2.0);
    // Replacing again drops the previous levels instead of stacking them.
    m.replace_liquidity("market", &ex, &[], &[(px(6000), 1)]);
    assert_eq!(m.open_orders_of("market").len(), 1);
    // Unknown exchanges are registered on the fly.
    assert_eq!(
        m.replace_liquidity("market", &ExchangeId::new("999"), &[(px(100), 1)], &[]),
        1
    );
}
