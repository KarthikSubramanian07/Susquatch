//! Realtime client against a local fake Phoenix / Supabase Realtime server.
//!
//! Every wait is bounded by a timeout, so a regression fails fast instead of hanging.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use susq_core::{ExchangeId, SystemClock};
use susq_platform::{
    PlatformError, PlatformResult, RealtimeToken,
    realtime::{RealtimeClient, RealtimeConfig, RealtimeEvent, ResyncReason, TokenSource},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
    },
};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(5);

// ───────────────────────────── fake server ─────────────────────────────

struct FakeServer {
    url: String,
    conns: mpsc::UnboundedReceiver<ServerConn>,
}

impl FakeServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let (tx, conns) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let uri = Arc::new(Mutex::new(String::new()));
                    let seen = Arc::clone(&uri);
                    // The signature is fixed by tungstenite's handshake `Callback` trait.
                    #[allow(clippy::result_large_err)]
                    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                        *seen.lock().expect("lock") = req.uri().to_string();
                        Ok(resp)
                    };
                    if let Ok(ws) = accept_hdr_async(stream, callback).await {
                        let uri = uri.lock().expect("lock").clone();
                        let _ = tx.send(ServerConn { ws, uri });
                    }
                });
            }
        });
        Self { url, conns }
    }

    async fn accept(&mut self) -> ServerConn {
        timeout(WAIT, self.conns.recv())
            .await
            .expect("client connected in time")
            .expect("server alive")
    }
}

struct ServerConn {
    ws: WebSocketStream<TcpStream>,
    uri: String,
}

impl ServerConn {
    /// Next text frame as JSON.
    async fn recv(&mut self) -> Value {
        loop {
            let msg = timeout(WAIT, self.ws.next())
                .await
                .expect("frame in time")
                .expect("socket open")
                .expect("socket ok");
            if let Message::Text(text) = msg {
                return serde_json::from_str(text.as_str()).expect("json frame");
            }
        }
    }

    async fn send(&mut self, v: Value) {
        self.ws
            .send(Message::text(v.to_string()))
            .await
            .expect("server send");
    }

    async fn reply(&mut self, to: &Value, status: &str) {
        self.send(json!({
            "topic": to["topic"], "event": "phx_reply", "ref": to["ref"], "join_ref": to["join_ref"],
            "payload": {"status": status, "response": {}}
        }))
        .await;
    }

    /// Receive `n` joins and accept them all.
    async fn accept_joins(&mut self, n: usize) -> Vec<Value> {
        let mut joins = Vec::new();
        for _ in 0..n {
            let join = self.recv().await;
            assert_eq!(join["event"], "phx_join", "{join}");
            self.reply(&join, "ok").await;
            joins.push(join);
        }
        joins
    }

    async fn broadcast(&mut self, channel: &str, event: &str, payload: Value) {
        self.send(json!({
            "topic": format!("realtime:{channel}"), "event": "broadcast", "ref": null,
            "payload": {"type": "broadcast", "event": event, "payload": payload}
        }))
        .await;
    }

    async fn market_batch(&mut self, channel: &str, revision: Value, previous: Value) {
        let payload = json!({
            "trades": [], "bookDirty": [], "marketSettled": [],
            "delivery": {"revision": revision, "previousRevision": previous}
        });
        self.broadcast(channel, "market_batch", payload).await;
    }
}

// ───────────────────────────── fake tokens & client ─────────────────────────────

type FailRule = Box<dyn Fn(usize) -> bool + Send + Sync>;

struct FakeTokens {
    url: String,
    lifetime: Duration,
    calls: AtomicUsize,
    fails: FailRule,
}

impl FakeTokens {
    fn new(url: &str, lifetime: Duration) -> Self {
        Self::failing(url, lifetime, |_| false)
    }

    fn failing(url: &str, lifetime: Duration, fails: impl Fn(usize) -> bool + Send + Sync + 'static) -> Self {
        Self {
            url: url.to_owned(),
            lifetime,
            calls: AtomicUsize::new(0),
            fails: Box::new(fails),
        }
    }
}

#[async_trait]
impl TokenSource for FakeTokens {
    async fn token(&self) -> PlatformResult<RealtimeToken> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if (self.fails)(n) {
            return Err(PlatformError::Network {
                message: format!("mint {n} refused"),
                timeout: false,
            });
        }
        let lifetime = chrono::Duration::from_std(self.lifetime).expect("lifetime");
        Ok(RealtimeToken {
            token: format!("tok-{n}"),
            expires_at: (Utc::now() + lifetime).to_rfc3339(),
            supabase_url: self.url.clone(),
            anon_key: "anon-key".into(),
            ..Default::default()
        })
    }
}

fn config(channels: &[&str]) -> RealtimeConfig {
    RealtimeConfig {
        channels: channels.iter().map(|c| (*c).to_owned()).collect(),
        heartbeat: Duration::from_secs(60),
        refresh_before: Duration::from_secs(1),
        min_backoff: Duration::from_millis(20),
        max_backoff: Duration::from_millis(100),
        join_timeout: Duration::from_secs(2),
        stable_after: Duration::from_secs(60),
        buffer: 64,
    }
}

struct Client {
    rx: mpsc::Receiver<RealtimeEvent>,
    handle: JoinHandle<()>,
    shutdown: CancellationToken,
}

impl Client {
    fn spawn(cfg: RealtimeConfig, tokens: FakeTokens) -> Self {
        let shutdown = CancellationToken::new();
        let (rx, handle) = RealtimeClient::spawn(cfg, tokens, Arc::new(SystemClock), shutdown.clone());
        Self { rx, handle, shutdown }
    }

    async fn next(&mut self) -> RealtimeEvent {
        timeout(WAIT, self.rx.recv())
            .await
            .expect("event in time")
            .expect("client alive")
    }

    async fn expect_subscribed(&mut self, reason: ResyncReason) {
        assert_eq!(self.next().await, RealtimeEvent::Connected);
        assert_eq!(self.next().await, RealtimeEvent::Resync { reason });
    }

    async fn stop(self) {
        self.shutdown.cancel();
        timeout(Duration::from_secs(1), self.handle)
            .await
            .expect("stopped promptly")
            .expect("task did not panic");
    }
}

fn resync(reason: ResyncReason) -> RealtimeEvent {
    RealtimeEvent::Resync { reason }
}

fn revision_of(event: &RealtimeEvent) -> Option<u64> {
    match event {
        RealtimeEvent::MarketBatch { batch, .. } => batch.delivery.revision,
        other => panic!("expected a market batch, got {other:?}"),
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[tokio::test]
async fn joins_private_channels_and_yields_typed_events() {
    let mut server = FakeServer::start().await;
    let channels = [
        "tournament:t1",
        "relationships:violations:t1",
        "user:p1",
        "leaderboard:all",
    ];
    let mut client = Client::spawn(
        config(&channels),
        FakeTokens::new(&server.url, Duration::from_secs(3 * 3600)),
    );
    let mut conn = server.accept().await;
    assert_eq!(conn.uri, "/realtime/v1/websocket?apikey=anon-key&vsn=1.0.0");

    let joins = conn.accept_joins(channels.len()).await;
    for (join, channel) in joins.iter().zip(channels) {
        assert_eq!(join["topic"], format!("realtime:{channel}"));
        assert_eq!(join["ref"], join["join_ref"]);
        assert_eq!(join["payload"]["access_token"], "tok-1");
        assert_eq!(
            join["payload"]["config"],
            json!({"broadcast": {"ack": false, "self": false}, "presence": {"key": ""}, "private": true})
        );
    }
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;

    conn.broadcast(
        "tournament:t1",
        "market_batch",
        json!({"trades": [{"exchangeId": 381, "marketId": "m1", "price": 0.42, "quantity": 5,
                           "executedAt": "2026-10-01T16:00:01Z", "tournamentId": "t1"}],
               "bookDirty": [{"exchangeId": "381", "marketId": "m1", "tournamentId": "t1"}],
               "delivery": {"revision": 1, "previousRevision": null}}),
    )
    .await;
    conn.broadcast(
        "user:p1",
        "account_batch",
        json!({"fills": [{"orderId": 7, "exchangeId": "381", "price": 0.42, "quantity": 5}],
               "orderUpdates": [{"orderId": 7, "exchangeId": "381", "open": false, "quantityTraded": 5}],
               "delivery": {"revision": 1}}),
    )
    .await;
    conn.broadcast(
        "relationships:violations:t1",
        "violation",
        json!({"relationshipId": "r1", "type": "mutually_exclusive", "violationAmount": 0.07,
               "evaluationStatus": "violated"}),
    )
    .await;
    conn.broadcast("leaderboard:all", "tick", json!({"at": "2026-10-01T16:00:05Z"}))
        .await;

    match client.next().await {
        RealtimeEvent::MarketBatch { channel, batch } => {
            assert_eq!(channel, "tournament:t1");
            assert_eq!(batch.trades[0].exchange_id, ExchangeId::new("381"));
            assert_eq!(batch.book_dirty.len(), 1);
        }
        other => panic!("expected market batch, got {other:?}"),
    }
    match client.next().await {
        RealtimeEvent::AccountBatch { channel, batch } => {
            assert_eq!(channel, "user:p1");
            assert_eq!(batch.fills[0].order_id, Some(7));
            assert!(!batch.order_updates[0].open);
        }
        other => panic!("expected account batch, got {other:?}"),
    }
    match client.next().await {
        RealtimeEvent::Violation { channel, eval } => {
            assert_eq!(channel, "relationships:violations:t1");
            assert_eq!(eval.relationship_id, "r1");
        }
        other => panic!("expected violation, got {other:?}"),
    }
    assert!(matches!(
        client.next().await,
        RealtimeEvent::LeaderboardTick { channel, tick } if channel == "leaderboard:all" && tick.at.is_some()
    ));
    client.stop().await;
}

#[tokio::test]
async fn revision_gap_resyncs_before_the_batch_and_duplicates_are_dropped() {
    let mut server = FakeServer::start().await;
    let mut client = Client::spawn(
        config(&["tournament:t1"]),
        FakeTokens::new(&server.url, Duration::from_secs(3600)),
    );
    let mut conn = server.accept().await;
    conn.accept_joins(1).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;

    conn.market_batch("tournament:t1", json!(1), Value::Null).await;
    conn.market_batch("tournament:t1", json!(2), json!(1)).await;
    conn.market_batch("tournament:t1", json!(2), json!(1)).await;
    conn.market_batch("tournament:t1", json!(5), json!(4)).await;
    conn.market_batch("tournament:t1", json!("6"), json!("5")).await;

    assert_eq!(revision_of(&client.next().await), Some(1));
    assert_eq!(revision_of(&client.next().await), Some(2));
    assert_eq!(
        client.next().await,
        resync(ResyncReason::RevisionGap {
            channel: "tournament:t1".into(),
            expected: 2,
            got: Some(4)
        })
    );
    assert_eq!(revision_of(&client.next().await), Some(5));
    assert_eq!(revision_of(&client.next().await), Some(6));
    client.stop().await;
}

#[tokio::test]
async fn dropped_connection_reconnects_rejoins_and_resyncs() {
    let mut server = FakeServer::start().await;
    let mut client = Client::spawn(
        config(&["tournament:t1", "user:p1"]),
        FakeTokens::new(&server.url, Duration::from_secs(3600)),
    );
    let mut conn = server.accept().await;
    conn.accept_joins(2).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;
    conn.market_batch("tournament:t1", json!(1), Value::Null).await;
    assert_eq!(revision_of(&client.next().await), Some(1));

    drop(conn);
    assert!(matches!(client.next().await, RealtimeEvent::Disconnected { .. }));
    let mut conn = server.accept().await;
    let joins = conn.accept_joins(2).await;
    assert_eq!(joins[0]["topic"], "realtime:tournament:t1");
    assert_eq!(joins[1]["topic"], "realtime:user:p1");
    assert_eq!(
        joins[0]["payload"]["access_token"], "tok-1",
        "a still-valid token is reused"
    );
    client.expect_subscribed(ResyncReason::Reconnect).await;

    conn.market_batch("tournament:t1", json!(10), json!(9)).await;
    assert_eq!(
        revision_of(&client.next().await),
        Some(10),
        "tracking was reset on reconnect"
    );
    client.stop().await;
}

#[tokio::test]
async fn missing_heartbeat_reply_reconnects() {
    let mut server = FakeServer::start().await;
    let cfg = RealtimeConfig {
        heartbeat: Duration::from_millis(150),
        ..config(&["tournament:t1"])
    };
    let mut client = Client::spawn(cfg, FakeTokens::new(&server.url, Duration::from_secs(3600)));
    let mut conn = server.accept().await;
    conn.accept_joins(1).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;

    let first = conn.recv().await;
    assert_eq!(first["topic"], "phoenix");
    assert_eq!(first["event"], "heartbeat");
    assert_eq!(first["payload"], json!({}));
    assert!(first["ref"].is_string());
    conn.reply(&first, "ok").await;
    let second = conn.recv().await;
    assert_eq!(second["event"], "heartbeat");
    assert_ne!(first["ref"], second["ref"]);

    match client.next().await {
        RealtimeEvent::Disconnected { reason } => assert!(reason.contains("heartbeat"), "{reason}"),
        other => panic!("expected disconnect, got {other:?}"),
    }
    let mut conn = server.accept().await;
    conn.accept_joins(1).await;
    client.expect_subscribed(ResyncReason::Reconnect).await;
    client.stop().await;
}

#[tokio::test]
async fn token_refresh_pushes_new_token_to_every_topic_and_resyncs() {
    let mut server = FakeServer::start().await;
    let cfg = RealtimeConfig {
        refresh_before: Duration::from_millis(1500),
        ..config(&["tournament:t1", "user:p1"])
    };
    let mut client = Client::spawn(cfg, FakeTokens::new(&server.url, Duration::from_secs(2)));
    let mut conn = server.accept().await;
    let joins = conn.accept_joins(2).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;
    conn.market_batch("tournament:t1", json!(1), Value::Null).await;
    assert_eq!(revision_of(&client.next().await), Some(1));

    for join in &joins {
        let push = conn.recv().await;
        assert_eq!(push["event"], "access_token");
        assert_eq!(push["topic"], join["topic"]);
        assert_eq!(push["join_ref"], join["join_ref"]);
        assert_eq!(push["payload"], json!({"access_token": "tok-2"}));
    }
    assert_eq!(client.next().await, resync(ResyncReason::TokenRefresh));

    conn.market_batch("tournament:t1", json!(10), json!(9)).await;
    assert_eq!(
        revision_of(&client.next().await),
        Some(10),
        "tracking was reset on refresh"
    );
    client.stop().await;
}

#[tokio::test]
async fn failed_refresh_keeps_the_socket_and_retries() {
    let mut server = FakeServer::start().await;
    let cfg = RealtimeConfig {
        refresh_before: Duration::from_millis(1500),
        ..config(&["user:p1"])
    };
    let tokens = FakeTokens::failing(&server.url, Duration::from_secs(2), |n| n == 2);
    let mut client = Client::spawn(cfg, tokens);
    let mut conn = server.accept().await;
    conn.accept_joins(1).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;

    let push = conn.recv().await;
    assert_eq!(push["event"], "access_token");
    assert_eq!(push["payload"], json!({"access_token": "tok-3"}));
    assert_eq!(client.next().await, resync(ResyncReason::TokenRefresh));
    client.stop().await;
}

#[tokio::test]
async fn expired_token_forces_a_reconnect() {
    let mut server = FakeServer::start().await;
    let cfg = RealtimeConfig {
        refresh_before: Duration::from_millis(100),
        ..config(&["user:p1"])
    };
    let tokens = FakeTokens::failing(&server.url, Duration::from_millis(600), |n| n >= 2);
    let mut client = Client::spawn(cfg, tokens);
    let mut conn = server.accept().await;
    conn.accept_joins(1).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;
    match client.next().await {
        RealtimeEvent::Disconnected { reason } => assert!(reason.contains("expired"), "{reason}"),
        other => panic!("expected disconnect, got {other:?}"),
    }
    client.stop().await;
}

#[tokio::test]
async fn errored_channel_is_rejoined_and_stale_notices_ignored() {
    let mut server = FakeServer::start().await;
    let mut client = Client::spawn(
        config(&["tournament:t1", "leaderboard:all"]),
        FakeTokens::new(&server.url, Duration::from_secs(3600)),
    );
    let mut conn = server.accept().await;
    let joins = conn.accept_joins(2).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;

    let old_ref = joins[0]["join_ref"].clone();
    conn.send(
        json!({"topic": "realtime:tournament:t1", "event": "phx_error", "ref": old_ref,
                     "join_ref": old_ref, "payload": {}}),
    )
    .await;
    let rejoin = conn.recv().await;
    assert_eq!(rejoin["event"], "phx_join");
    assert_eq!(rejoin["topic"], "realtime:tournament:t1");
    assert_ne!(rejoin["join_ref"], old_ref);
    conn.reply(&rejoin, "ok").await;
    assert_eq!(
        client.next().await,
        resync(ResyncReason::SocketError {
            channel: "tournament:t1".into()
        })
    );

    conn.send(
        json!({"topic": "realtime:tournament:t1", "event": "phx_close", "ref": null,
                     "join_ref": old_ref, "payload": {}}),
    )
    .await;
    conn.broadcast("leaderboard:all", "tick", json!({})).await;
    assert!(matches!(
        client.next().await,
        RealtimeEvent::LeaderboardTick { .. }
    ));
    client.stop().await;
}

#[tokio::test]
async fn rejected_join_does_not_block_other_channels_and_is_retried() {
    let mut server = FakeServer::start().await;
    let mut client = Client::spawn(
        config(&["tournament:t1", "user:p1"]),
        FakeTokens::new(&server.url, Duration::from_secs(3600)),
    );
    let mut conn = server.accept().await;
    let first = conn.recv().await;
    conn.reply(&first, "ok").await;
    let second = conn.recv().await;
    conn.reply(&second, "error").await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;

    let retry = conn.recv().await;
    assert_eq!(retry["event"], "phx_join");
    assert_eq!(retry["topic"], "realtime:user:p1");
    conn.reply(&retry, "ok").await;
    assert_eq!(
        client.next().await,
        resync(ResyncReason::SocketError {
            channel: "user:p1".into()
        })
    );
    client.stop().await;
}

#[tokio::test]
async fn shutdown_stops_a_connected_client_promptly() {
    let mut server = FakeServer::start().await;
    let mut client = Client::spawn(
        config(&["user:p1"]),
        FakeTokens::new(&server.url, Duration::from_secs(3600)),
    );
    let mut conn = server.accept().await;
    conn.accept_joins(1).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;
    client.stop().await;
}

#[tokio::test]
async fn shutdown_interrupts_reconnect_backoff() {
    let server = FakeServer::start().await;
    let cfg = RealtimeConfig {
        min_backoff: Duration::from_secs(30),
        max_backoff: Duration::from_secs(60),
        ..config(&["user:p1"])
    };
    let mut client = Client::spawn(
        cfg,
        FakeTokens::failing(&server.url, Duration::from_secs(3600), |_| true),
    );
    match client.next().await {
        RealtimeEvent::Disconnected { reason } => assert!(reason.contains("mint"), "{reason}"),
        other => panic!("expected disconnect, got {other:?}"),
    }
    client.stop().await;
}

#[tokio::test]
async fn dropping_the_receiver_stops_the_task() {
    let mut server = FakeServer::start().await;
    let mut client = Client::spawn(
        config(&["user:p1"]),
        FakeTokens::new(&server.url, Duration::from_secs(3600)),
    );
    let mut conn = server.accept().await;
    conn.accept_joins(1).await;
    client.expect_subscribed(ResyncReason::InitialSubscribe).await;
    let Client { rx, handle, .. } = client;
    drop(rx);
    timeout(Duration::from_secs(1), handle)
        .await
        .expect("stopped promptly")
        .expect("task did not panic");
}
