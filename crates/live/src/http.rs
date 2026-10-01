//! Loopback-only `/healthz` and Prometheus `/metrics`.

use std::{net::SocketAddr, sync::Arc};

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use metrics_exporter_prometheus::PrometheusHandle;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// What `/healthz` reports; the engine loop updates it every step.
#[derive(Debug, Clone, Default)]
pub struct Liveness {
    /// Last completed engine step (UTC ns).
    pub last_step_ns: i64,
    /// Current time as the loop sees it (UTC ns).
    pub now_ns: i64,
    /// A step older than this is "stuck".
    pub max_step_age_ns: i64,
    /// First contact (REST bootstrap) still running: healthy by definition, no step has happened yet.
    pub bootstrapping: bool,
    pub body: Value,
}

#[derive(Clone)]
struct AppState {
    liveness: Arc<Mutex<Liveness>>,
    metrics: Option<PrometheusHandle>,
}

async fn healthz(State(s): State<AppState>) -> impl IntoResponse {
    let l = s.liveness.lock().clone();
    let now = susq_core::Clock::now_ns(&susq_core::SystemClock).max(l.now_ns);
    let age = now - l.last_step_ns;
    let ok = l.bootstrapping || (l.last_step_ns > 0 && age <= l.max_step_age_ns);
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "ok": ok,
            "bootstrapping": l.bootstrapping,
            "last_step_age_ms": age / 1_000_000,
            "detail": l.body
        })),
    )
}

async fn metrics(State(s): State<AppState>) -> impl IntoResponse {
    match s.metrics {
        Some(h) => (StatusCode::OK, h.render()),
        None => (StatusCode::NOT_FOUND, "metrics recorder not installed".to_owned()),
    }
}

/// The router (exposed for tests).
pub fn router(liveness: Arc<Mutex<Liveness>>, metrics_handle: Option<PrometheusHandle>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .with_state(AppState {
            liveness,
            metrics: metrics_handle,
        })
}

/// Serve on `addr` until `shutdown`. Refuses non-loopback addresses: these endpoints are for
/// the local status pusher and an SSH-tunnelled Grafana only.
pub async fn serve(
    addr: SocketAddr,
    liveness: Arc<Mutex<Liveness>>,
    metrics_handle: Option<PrometheusHandle>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "metrics_addr {addr} must be a loopback address"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(liveness, metrics_handle))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn healthz_reflects_the_loop() {
        let live = Arc::new(Mutex::new(Liveness::default()));
        let shutdown = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let app = router(live.clone(), None);
        let s = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { s.cancelled().await })
                .await
        });
        let get = |path: &'static str| async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
            stream.write_all(req.as_bytes()).await.expect("write");
            let mut out = String::new();
            stream.read_to_string(&mut out).await.expect("read");
            out
        };
        assert!(get("/healthz").await.starts_with("HTTP/1.1 503"), "no step yet");
        live.lock().bootstrapping = true;
        assert!(
            get("/healthz").await.starts_with("HTTP/1.1 200"),
            "healthy while bootstrapping"
        );
        live.lock().bootstrapping = false;
        {
            let mut l = live.lock();
            l.last_step_ns = susq_core::Clock::now_ns(&susq_core::SystemClock);
            l.max_step_age_ns = 60_000_000_000;
        }
        assert!(get("/healthz").await.starts_with("HTTP/1.1 200"));
        assert!(get("/metrics").await.starts_with("HTTP/1.1 404"));
        shutdown.cancel();
        server.await.expect("join").expect("serve");
    }

    #[tokio::test]
    async fn refuses_public_addresses() {
        let r = serve(
            "0.0.0.0:0".parse().expect("addr"),
            Arc::default(),
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(r.is_err());
    }
}
