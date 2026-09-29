//! The real REST adapter for The Super Market (`https://www.thesuper.market/api/v1`).
//!
//! One attempt per call. Every request and response is journaled (path + body, never the key,
//! which travels only in the `Authorization` header). Errors come back typed with `Retry-After`
//! and `x-request-id` so the gateway can apply the documented retry policy.

use std::{sync::Arc, time::Duration, time::Instant};

use async_trait::async_trait;
use reqwest::{Method, StatusCode, header::HeaderMap};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use susq_contracts::v1::JournalEntry;
use susq_core::{Clock, ExchangeId, OrderId, Secret, TournamentId};
use susq_journal::Journal;
use url::Url;

use crate::{
    adapter::PlatformAdapter,
    error::{ApiError, PlatformError, PlatformResult},
    models::*,
};

/// Connection settings.
#[derive(Debug, Clone)]
pub struct SuperMarketConfig {
    /// e.g. `https://www.thesuper.market/api/v1`
    pub base_url: String,
    pub api_key: Secret,
    pub timeout: Duration,
}

/// REST client for the official API.
#[derive(Debug, Clone)]
pub struct SuperMarket {
    http: reqwest::Client,
    base: Url,
    key: Secret,
    journal: Journal,
    clock: Arc<dyn Clock>,
}

/// Raw response pieces before interpretation.
struct Raw {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl SuperMarket {
    /// Build the client. Fails on an unparseable base URL or TLS setup error.
    pub fn new(cfg: SuperMarketConfig, journal: Journal, clock: Arc<dyn Clock>) -> PlatformResult<Self> {
        let mut base = Url::parse(cfg.base_url.trim_end_matches('/'))
            .map_err(|e| PlatformError::Invalid(format!("base_url: {e}")))?;
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("susquatch-rust-live/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| PlatformError::Invalid(format!("http client: {e}")))?;
        Ok(Self {
            http,
            base,
            key: cfg.api_key,
            journal,
            clock,
        })
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> PlatformResult<Url> {
        let mut url = self
            .base
            .join(path.trim_start_matches('/'))
            .map_err(|e| PlatformError::Invalid(format!("path {path}: {e}")))?;
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    fn journal_path(url: &Url) -> String {
        match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        }
    }

    async fn record(&self, entry: JournalEntry) {
        // Backpressure is bounded by the journal channel; a closed journal must not stop trading,
        // but it is visible through the journal's health (see live).
        let _ = self.journal.append(entry).await;
    }

    /// One HTTP exchange, journaled on both sides. Transport failures become `Network`.
    async fn exchange(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        idempotency_key: Option<&str>,
    ) -> PlatformResult<Raw> {
        let url = self.url(path, query)?;
        let jpath = Self::journal_path(&url);
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let body_text = body.as_ref().map(Value::to_string).unwrap_or_default();
        self.record(JournalEntry {
            ts_ns: self.clock.now_ns(),
            direction: "request".into(),
            method: method.to_string(),
            path: jpath.clone(),
            body_json: body_text.clone(),
            idempotency_key: idempotency_key.unwrap_or_default().to_owned(),
            correlation_id: correlation_id.clone(),
            ..Default::default()
        })
        .await;

        let mut req = self
            .http
            .request(method.clone(), url)
            .bearer_auth(self.key.expose())
            .header("accept", "application/json");
        if body.is_some() {
            req = req.header("content-type", "application/json").body(body_text);
        }
        let started = Instant::now();
        let result = req.send().await;
        let latency_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);

        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                let timeout = e.is_timeout();
                let err = PlatformError::Network {
                    message: e.without_url().to_string(),
                    timeout,
                };
                self.record(JournalEntry {
                    ts_ns: self.clock.now_ns(),
                    direction: "error".into(),
                    method: method.to_string(),
                    path: jpath,
                    body_json: json!({ "error": err.to_string() }).to_string(),
                    latency_us,
                    idempotency_key: idempotency_key.unwrap_or_default().to_owned(),
                    correlation_id,
                    ..Default::default()
                })
                .await;
                return Err(err);
            }
        };
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await.map_err(|e| PlatformError::Decode {
            status: status.as_u16(),
            message: format!("reading body: {}", e.without_url()),
        })?;
        self.record(JournalEntry {
            ts_ns: self.clock.now_ns(),
            direction: "response".into(),
            method: method.to_string(),
            path: jpath,
            body_json: body.clone(),
            status: u32::from(status.as_u16()),
            latency_us,
            request_id: request_id(&headers).unwrap_or_default(),
            idempotency_key: idempotency_key.unwrap_or_default().to_owned(),
            correlation_id,
            ..Default::default()
        })
        .await;
        Ok(Raw {
            status,
            headers,
            body,
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> PlatformResult<T> {
        let raw = self.exchange(Method::GET, path, query, None, None).await?;
        expect_success(raw)
    }

    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        idempotency_key: Option<&str>,
    ) -> PlatformResult<T> {
        let raw = self
            .exchange(Method::POST, path, &[], Some(to_value(body)?), idempotency_key)
            .await?;
        expect_success(raw)
    }
}

fn to_value<B: Serialize>(b: &B) -> PlatformResult<Value> {
    serde_json::to_value(b).map_err(|e| PlatformError::Invalid(format!("encoding body: {e}")))
}

fn request_id(h: &HeaderMap) -> Option<String> {
    h.get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// `Retry-After` in its delta-seconds form (the platform's form); HTTP-dates are ignored.
fn retry_after(h: &HeaderMap) -> Option<Duration> {
    let v = h.get("retry-after")?.to_str().ok()?.trim().parse::<f64>().ok()?;
    (v.is_finite() && v >= 0.0).then(|| Duration::from_secs_f64(v.min(3600.0)))
}

/// Build the typed error for a non-success response. Bodies that are not the standard envelope
/// (e.g. a batch 429/502/503 carrying per-item `results`) keep the body in `details`.
fn api_error(raw: &Raw) -> ApiError {
    let parsed: Option<Value> = serde_json::from_str(&raw.body).ok();
    let envelope = parsed.as_ref().and_then(|v| v.get("error"));
    let code = envelope
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| batch_code(raw.status.as_u16(), parsed.as_ref()))
        .unwrap_or_else(|| format!("HTTP_{}", raw.status.as_u16()));
    let message = envelope
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let details = envelope
        .and_then(|e| e.get("details"))
        .cloned()
        .or_else(|| parsed.clone().filter(|v| v.get("results").is_some()))
        .unwrap_or(Value::Null);
    ApiError {
        status: raw.status.as_u16(),
        code,
        message,
        details,
        retry_after: retry_after(&raw.headers),
        request_id: request_id(&raw.headers),
    }
}

/// Batch-level codes for the documented zero-success envelopes.
fn batch_code(status: u16, body: Option<&Value>) -> Option<String> {
    body?.get("results")?;
    match status {
        429 => Some("RATE_LIMITED".into()),
        502 => Some("ORDER_STATUS_UNKNOWN".into()),
        503 => Some("SERVICE_UNAVAILABLE".into()),
        _ => None,
    }
}

fn decode<T: DeserializeOwned>(raw: &Raw) -> PlatformResult<T> {
    serde_json::from_str(&raw.body).map_err(|e| PlatformError::Decode {
        status: raw.status.as_u16(),
        message: e.to_string(),
    })
}

fn expect_success<T: DeserializeOwned>(raw: Raw) -> PlatformResult<T> {
    if raw.status.is_success() {
        decode(&raw)
    } else {
        Err(api_error(&raw).into())
    }
}

/// Path segments are ids or slugs; refuse anything that could change the path.
fn segment(s: &str) -> PlatformResult<&str> {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(s)
    } else {
        Err(PlatformError::Invalid(format!("unsafe path segment {s:?}")))
    }
}

fn tid_q(tid: &TournamentId) -> (&'static str, String) {
    ("tournamentId", tid.to_string())
}

#[async_trait]
impl PlatformAdapter for SuperMarket {
    async fn tournament(&self, slug: &str) -> PlatformResult<Tournament> {
        self.get(&format!("tournaments/{}", segment(slug)?), &[]).await
    }

    async fn markets_page(
        &self,
        slug: &str,
        status: MarketStatusFilter,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Market>> {
        let mut q = vec![
            ("status", status.as_str().to_owned()),
            ("limit", "100".to_owned()),
        ];
        if let Some(c) = cursor {
            q.push(("cursor", c.to_owned()));
        }
        self.get(&format!("tournaments/{}/markets", segment(slug)?), &q)
            .await
    }

    async fn prices(&self, tid: &TournamentId, ids: &[ExchangeId]) -> PlatformResult<BulkPrices> {
        if ids.is_empty() {
            return Ok(BulkPrices::default());
        }
        if ids.len() > MAX_PRICE_IDS {
            return Err(PlatformError::Invalid(format!(
                "{} ids > {MAX_PRICE_IDS}",
                ids.len()
            )));
        }
        let joined = ids.iter().map(ExchangeId::as_str).collect::<Vec<_>>().join(",");
        self.get("exchanges/prices", &[("ids", joined), tid_q(tid)]).await
    }

    async fn orderbook(
        &self,
        tid: &TournamentId,
        exchange: &ExchangeId,
        depth: u32,
    ) -> PlatformResult<OrderBook> {
        let q = [("depth", depth.clamp(1, 200).to_string()), tid_q(tid)];
        self.get(
            &format!("exchanges/{}/orderbook", segment(exchange.as_str())?),
            &q,
        )
        .await
    }

    async fn place_order(&self, req: &PlaceOrderRequest) -> PlatformResult<OrderAck> {
        self.post("orders", req, Some(&req.idempotency_key)).await
    }

    async fn place_batch(&self, req: &BatchRequest) -> PlatformResult<BatchResponse> {
        if req.orders.is_empty() || req.orders.len() > MAX_BATCH {
            return Err(PlatformError::Invalid(format!(
                "batch of {} orders",
                req.orders.len()
            )));
        }
        let raw = self
            .exchange(
                Method::POST,
                "orders/batch",
                &[],
                Some(to_value(req)?),
                Some(&req.idempotency_key),
            )
            .await?;
        match raw.status.as_u16() {
            200 | 207 | 422 => {
                let mut resp: BatchResponse = decode(&raw)?;
                resp.http_status = raw.status.as_u16();
                Ok(resp)
            }
            _ => Err(api_error(&raw).into()),
        }
    }

    async fn place_multi_leg(&self, req: &MultiLegRequest) -> PlatformResult<MultiLegResponse> {
        if req.legs.is_empty() || req.legs.len() > MAX_LEGS {
            return Err(PlatformError::Invalid(format!(
                "multi-leg of {} legs",
                req.legs.len()
            )));
        }
        self.post("orders/multi-leg", req, Some(&req.idempotency_key))
            .await
    }

    async fn cancel_order(&self, id: OrderId) -> PlatformResult<CancelOutcome> {
        let raw = self
            .exchange(Method::DELETE, &format!("orders/{}", id.0), &[], None, None)
            .await?;
        match raw.status.as_u16() {
            200..=299 => Ok(CancelOutcome::Cancelled),
            409 => Ok(CancelOutcome::AlreadyClosed),
            _ => Err(api_error(&raw).into()),
        }
    }

    async fn cancel_all(&self, tid: &TournamentId, scope: &CancelScope) -> PlatformResult<CancelAllResponse> {
        if scope.exchange_id.is_some() && scope.market_id.is_some() {
            return Err(PlatformError::Invalid(
                "exchangeId and marketId are mutually exclusive".into(),
            ));
        }
        let mut body = json!({ "tournamentId": tid });
        if let Some(e) = &scope.exchange_id {
            body["exchangeId"] = json!(e);
        }
        if let Some(m) = &scope.market_id {
            body["marketId"] = json!(m);
        }
        let raw = self
            .exchange(Method::POST, "orders/cancel-all", &[], Some(body), None)
            .await?;
        match raw.status.as_u16() {
            200 | 207 | 422 => {
                let mut resp: CancelAllResponse = decode(&raw)?;
                resp.http_status = raw.status.as_u16();
                Ok(resp)
            }
            _ => Err(api_error(&raw).into()),
        }
    }

    async fn orders_page(
        &self,
        tid: &TournamentId,
        status: OrderStatusFilter,
        exchange: Option<&ExchangeId>,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Order>> {
        let mut q = vec![
            ("status", status.as_str().to_owned()),
            tid_q(tid),
            ("limit", "200".to_owned()),
        ];
        if let Some(e) = exchange {
            q.push(("exchangeId", e.to_string()));
        }
        if let Some(c) = cursor {
            q.push(("cursor", c.to_owned()));
        }
        self.get("orders", &q).await
    }

    async fn positions(&self, slug: &str) -> PlatformResult<Positions> {
        self.get(
            &format!("tournaments/{}/portfolio/positions", segment(slug)?),
            &[],
        )
        .await
    }

    async fn pnl(&self, slug: &str, period: PnlPeriod) -> PlatformResult<Pnl> {
        self.get(
            &format!("tournaments/{}/portfolio/pnl", segment(slug)?),
            &[("period", period.as_str().into())],
        )
        .await
    }

    async fn fills_page(&self, slug: &str, cursor: Option<&str>) -> PlatformResult<Page<Fill>> {
        let mut q = vec![("limit", "200".to_owned())];
        if let Some(c) = cursor {
            q.push(("cursor", c.to_owned()));
        }
        self.get(&format!("tournaments/{}/portfolio/fills", segment(slug)?), &q)
            .await
    }

    async fn leaderboard(
        &self,
        slug: &str,
        period: LeaderboardPeriod,
        limit: u32,
        offset: u32,
    ) -> PlatformResult<Leaderboard> {
        let q = [
            ("period", period.as_str().to_owned()),
            ("limit", limit.clamp(1, 100).to_string()),
            ("offset", offset.to_string()),
        ];
        self.get(&format!("tournaments/{}/leaderboard", segment(slug)?), &q)
            .await
    }

    async fn relationships_page(
        &self,
        tid: &TournamentId,
        cursor: Option<&str>,
    ) -> PlatformResult<Page<Relationship>> {
        let mut q = vec![tid_q(tid), ("limit", "200".to_owned())];
        if let Some(c) = cursor {
            q.push(("cursor", c.to_owned()));
        }
        self.get("relationships", &q).await
    }

    async fn constraints(
        &self,
        tid: &TournamentId,
        violations_only: bool,
        min_violation: f64,
    ) -> PlatformResult<Constraints> {
        let q = [
            ("violationsOnly", violations_only.to_string()),
            ("minViolation", min_violation.clamp(0.0, 1.0).to_string()),
            tid_q(tid),
        ];
        self.get("relationships/constraints", &q).await
    }

    async fn realtime_token(&self) -> PlatformResult<RealtimeToken> {
        self.post("realtime/token", &json!({}), None).await
    }

    async fn collateral(&self, tid: &TournamentId) -> PlatformResult<Collateral> {
        self.get("portfolio/collateral", &[tid_q(tid)]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use susq_core::{Action, Pips, Qty, Side, SystemClock};
    use susq_journal::JournalConfig;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path, query_param},
    };

    const KEY: &str = "unit-test-key";

    struct Harness {
        server: MockServer,
        sm: SuperMarket,
        journal_dir: tempfile::TempDir,
        journal: Journal,
        writer: std::thread::JoinHandle<std::io::Result<u64>>,
    }

    async fn harness() -> Harness {
        let server = MockServer::start().await;
        let journal_dir = tempfile::tempdir().expect("tmp");
        let (journal, writer) = Journal::start(JournalConfig {
            dir: journal_dir.path().to_owned(),
            producer: "rust-live".into(),
            capacity: 256,
            redact: vec![KEY.into()],
        })
        .expect("journal");
        let sm = SuperMarket::new(
            SuperMarketConfig {
                base_url: format!("{}/api/v1", server.uri()),
                api_key: Secret::new(KEY),
                timeout: Duration::from_secs(5),
            },
            journal.clone(),
            Arc::new(SystemClock),
        )
        .expect("client");
        Harness {
            server,
            sm,
            journal_dir,
            journal,
            writer,
        }
    }

    impl Harness {
        /// Close the journal and return every line written.
        fn journal_text(self) -> String {
            drop(self.sm);
            drop(self.journal);
            self.writer.join().expect("thread").expect("io");
            let mut text = String::new();
            for day in std::fs::read_dir(self.journal_dir.path()).expect("dir") {
                let f = day.expect("entry").path().join("rust-live.journal.jsonl");
                text.push_str(&std::fs::read_to_string(f).expect("file"));
            }
            text
        }
    }

    fn order() -> PlaceOrderRequest {
        let exp: DateTime<Utc> = "2026-10-09T00:00:30Z".parse().expect("ts");
        PlaceOrderRequest {
            order: OrderRequest::limit(
                ExchangeId::new("381"),
                Side::No,
                Action::Buy,
                Qty(37),
                Pips::new(1450).expect("valid"),
                exp,
                TournamentId::new("t-uuid"),
            ),
            idempotency_key: "idem-1".into(),
        }
    }

    #[tokio::test]
    async fn places_an_order_with_auth_and_exact_body() {
        let h = harness().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/orders"))
            .and(header("authorization", format!("Bearer {KEY}").as_str()))
            .and(body_json(json!({
                "exchangeId": "381", "side": "no", "action": "buy", "quantity": 37, "price": 0.145,
                "expirationDate": "2026-10-09T00:00:30Z", "tournamentId": "t-uuid",
                "idempotencyKey": "idem-1"
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-request-id", "req-1")
                    .set_body_json(json!({
                        "orderId": 1001, "exchangeId": "381", "open": true, "remainingQuantity": 37,
                        "quantityTraded": 0, "totalCost": 0, "fillPrice": null, "all": null
                    })),
            )
            .expect(1)
            .mount(&h.server)
            .await;
        let ack = h.sm.place_order(&order()).await.expect("placed");
        assert_eq!(ack.order_id, Some(1001));
        let text = h.journal_text();
        assert!(!text.contains(KEY), "API key must never reach the journal");
        assert!(text.contains("\"direction\":\"request\"") && text.contains("\"direction\":\"response\""));
        assert!(text.contains("req-1"));
    }

    #[tokio::test]
    async fn typed_errors_with_retry_after_and_request_id() {
        let h = harness().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/orders"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "3")
                    .insert_header("x-request-id", "req-429")
                    .set_body_json(json!({"error": {"code": "RATE_LIMITED", "message": "slow down"}})),
            )
            .mount(&h.server)
            .await;
        let err = h.sm.place_order(&order()).await.expect_err("429");
        let PlatformError::Api(api) = &err else {
            panic!("expected api error, got {err:?}")
        };
        assert_eq!(api.status, 429);
        assert_eq!(api.code, "RATE_LIMITED");
        assert_eq!(api.retry_after, Some(Duration::from_secs(3)));
        assert_eq!(api.request_id.as_deref(), Some("req-429"));
        assert_eq!(
            err.retry_class(),
            crate::RetryClass::Retry {
                min_wait: Some(Duration::from_secs(3))
            }
        );
    }

    #[tokio::test]
    async fn non_envelope_error_bodies_still_classify() {
        let h = harness().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/tournaments/midterm-elections"))
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
            .mount(&h.server)
            .await;
        let err = h.sm.tournament("midterm-elections").await.expect_err("502");
        assert_eq!(err.code(), Some("HTTP_502"));
        assert_eq!(err.retry_class(), crate::RetryClass::Unknown);
    }

    #[tokio::test]
    async fn batch_partial_and_zero_success_envelopes() {
        let h = harness().await;
        let body = json!({"results": [
            {"index": 0, "ok": true, "status": 200, "data": {"orderId": 1, "exchangeId": "381", "open": true, "quantityTraded": 0, "totalCost": 0}},
            {"index": 1, "ok": false, "status": 429, "data": {"code": "RATE_LIMITED"}}
        ]});
        Mock::given(method("POST"))
            .and(path("/api/v1/orders/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(body))
            .up_to_n_times(1)
            .mount(&h.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/orders/batch"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({"results": [
                {"index": 0, "ok": false, "status": 503, "data": {"code": "SERVICE_UNAVAILABLE"}}
            ]})))
            .mount(&h.server)
            .await;
        let req = BatchRequest {
            idempotency_key: "b1".into(),
            orders: vec![order().order, order().order],
        };
        let resp = h.sm.place_batch(&req).await.expect("207 is ok");
        assert_eq!(resp.http_status, 207);
        assert_eq!(resp.results[1].error_code().as_deref(), Some("RATE_LIMITED"));
        let err = h.sm.place_batch(&req).await.expect_err("503");
        assert_eq!(err.code(), Some("SERVICE_UNAVAILABLE"));
        assert!(matches!(err.retry_class(), crate::RetryClass::Retry { .. }));
    }

    #[tokio::test]
    async fn cancel_409_is_already_closed() {
        let h = harness().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v1/orders/77"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(json!({"error": {"code": "CONFLICT", "message": "closed"}})),
            )
            .mount(&h.server)
            .await;
        assert_eq!(
            h.sm.cancel_order(OrderId(77)).await,
            Ok(CancelOutcome::AlreadyClosed)
        );
    }

    #[tokio::test]
    async fn reads_pass_tournament_and_decode_tolerantly() {
        let h = harness().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/exchanges/381/orderbook"))
            .and(query_param("tournamentId", "t-uuid"))
            .and(query_param("depth", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "exchangeId": "381", "marketId": "9", "depth": 50,
                "bids": [{"price": 0.45, "quantity": 10}], "asks": [{"price": 0.5, "quantity": 4}],
                "bestBid": 0.45, "bestAsk": 0.5, "spread": 0.05, "somethingNew": 1
            })))
            .mount(&h.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/orders"))
            .and(query_param("status", "open"))
            .and(query_param("tournamentId", "t-uuid"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": 5, "exchangeId": "381", "side": "yes", "action": "buy", "quantity": 3,
                          "priceLimit": 0.4, "open": true, "createdAt": "x", "expirationDate": null}],
                "pagination": {"limit": 200, "hasMore": false, "nextCursor": null}
            })))
            .mount(&h.server)
            .await;
        let tid = TournamentId::new("t-uuid");
        let book =
            h.sm.orderbook(&tid, &ExchangeId::new("381"), 50)
                .await
                .expect("book");
        assert_eq!(book.asks[0].quantity, 4.0);
        let orders = crate::all_orders(&h.sm, &tid, OrderStatusFilter::Open, None)
            .await
            .expect("orders");
        assert_eq!(orders.len(), 1);
    }

    #[tokio::test]
    async fn bulk_prices_chunk_at_100() {
        let h = harness().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/exchanges/prices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [], "missingIds": []})))
            .expect(3)
            .mount(&h.server)
            .await;
        let ids: Vec<ExchangeId> = (0..250).map(|i| ExchangeId::new(i.to_string())).collect();
        let tid = TournamentId::new("t-uuid");
        crate::bulk_prices(&h.sm, &tid, &ids).await.expect("prices");
        assert!(matches!(
            h.sm.prices(&tid, &ids).await,
            Err(PlatformError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn network_errors_are_retryable_and_journaled() {
        let h = harness().await;
        let dead = SuperMarket::new(
            SuperMarketConfig {
                base_url: "http://127.0.0.1:9/api/v1".into(),
                api_key: Secret::new(KEY),
                timeout: Duration::from_millis(500),
            },
            h.journal.clone(),
            Arc::new(SystemClock),
        )
        .expect("client");
        let err = dead.place_order(&order()).await.expect_err("refused");
        assert!(matches!(err, PlatformError::Network { .. }));
        assert_eq!(err.retry_class(), crate::RetryClass::Retry { min_wait: None });
        drop(dead);
        let text = h.journal_text();
        assert!(text.contains("\"direction\":\"error\""));
        assert!(!text.contains(KEY));
    }

    #[test]
    fn unsafe_segments_are_refused() {
        assert!(segment("../admin").is_err());
        assert!(segment("midterm-elections").is_ok());
        assert!(segment("").is_err());
    }
}
