//! The subcommands: `run`, `paper-run`, `smoke`, `cancel-all`, `lease`.

use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::Context;
use futures::StreamExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use parking_lot::Mutex;
use serde_json::json;
use susq_bus::{
    Bus,
    monitor::{MonitorCfg, NeedleMonitor, NeedleSnapshot},
    watchdog::WatchdogCfg,
};
use susq_contracts::v1::MarketMap;
use susq_core::{
    Action, ExchangeId, Mode, OrderBundle, OrderSpec, Pips, Qty, Side, TournamentId, clock::from_ns,
};
use susq_gateway::KillSwitch;
use susq_oms::OrderState;
use susq_platform::realtime::{RealtimeClient, RealtimeConfig, tournament_channels};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    cli::{Cli, RunArgs},
    election,
    engine::Engine,
    http::{self, Liveness},
    realtime::{RealtimeFeed, Spawner},
    runtime::{Runtime, connect},
    status,
    wiring::{BuildOpts, RunPaths, build, host_name},
};

fn build_engine(
    rt: &Runtime,
    mode: Mode,
    realtime: Option<RealtimeFeed>,
    needle: Option<watch::Receiver<NeedleSnapshot>>,
    kill: Option<KillSwitch>,
) -> anyhow::Result<Engine> {
    build(BuildOpts {
        cfg: rt.cfg.clone(),
        home: rt.home.clone(),
        mode,
        clock: rt.clock.clone(),
        platform: rt.platform.clone(),
        journal: rt.journal.clone(),
        tournament: rt.tid(),
        paper_balance: rt.tournament.my_balance,
        realtime,
        needle,
        host: host_name(),
        pid: std::process::id(),
        kill,
    })
}

/// Race ids in the market map (needle posteriors for unknown races are dropped).
fn known_races(rt: &Runtime) -> HashSet<String> {
    std::fs::read_to_string(rt.home.join(&rt.cfg.paths.market_map))
        .ok()
        .and_then(|t| susq_contracts::from_json::<MarketMap>(&t).ok())
        .map(|m| {
            m.markets
                .into_iter()
                .map(|r| r.race_id)
                .filter(|r| !r.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The realtime feed: tournament-scope channels plus up to `live.realtime_market_channels`
/// per-market channels, which the engine picks and rebuilds as its markets of interest change.
/// Nothing connects until the engine's first market selection.
async fn start_realtime(
    rt: &Runtime,
    tid: &TournamentId,
    shutdown: &CancellationToken,
) -> Option<RealtimeFeed> {
    let token = match rt.platform.realtime_token().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "realtime unavailable; polling only");
            return None;
        }
    };
    let base = tournament_channels(tid, &token.channels.user);
    let rc = rt.cfg.realtime.clone();
    let (platform, clock, shutdown) = (rt.platform.clone(), rt.clock.clone(), shutdown.clone());
    let spawn: Spawner = Box::new(move |channels| {
        let cfg = RealtimeConfig {
            heartbeat: Duration::from_secs(rc.heartbeat_s),
            refresh_before: Duration::from_secs(rc.refresh_before_s),
            max_backoff: Duration::from_secs(rc.max_backoff_s),
            ..RealtimeConfig::new(channels)
        };
        let cancel = shutdown.child_token();
        let (rx, _task) = RealtimeClient::spawn(cfg, platform.clone(), clock.clone(), cancel.clone());
        (rx, cancel)
    });
    Some(RealtimeFeed::new(
        tid.clone(),
        base,
        rt.cfg.live.realtime_market_channels,
        spawn,
    ))
}

/// Connect to NATS, wire `ops.kill` to `kill`, and start the needle monitor. Returns the bus
/// (for the `--election` publisher) and the needle snapshots.
async fn start_bus(
    rt: &Runtime,
    kill: KillSwitch,
    shutdown: &CancellationToken,
) -> (Option<Arc<Bus>>, Option<watch::Receiver<NeedleSnapshot>>) {
    let bus = match Bus::connect(&rt.cfg.bus.nats_url, crate::wiring::PRODUCER, &status::git_sha()).await {
        Ok(b) => Arc::new(b),
        Err(e) => {
            tracing::warn!(error = %e, "NATS unavailable: needle is absent, election-night executor stays idle");
            return (None, None);
        }
    };
    if let Ok(mut kills) = bus.subscribe_kill().await {
        let cancel = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    k = kills.next() => match k {
                        Some(k) => kill.trigger(format!("ops.kill from {}: {}", k.issuer, k.reason)),
                        None => break,
                    },
                }
            }
        });
    }
    let Ok(messages) = bus.subscribe_needle().await else {
        return (Some(bus), None);
    };
    let monitor = NeedleMonitor::spawn(
        messages,
        known_races(rt),
        MonitorCfg::new(WatchdogCfg::from_config(&rt.cfg.needle)),
        rt.clock.clone(),
        shutdown.clone(),
    );
    (Some(bus), Some(monitor.subscribe()))
}

async fn terminate_signal() {
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        () = term => {},
    }
}

/// `run` / `paper-run`: the trading loop until SIGTERM / Ctrl-C.
pub async fn run(cli: &Cli, args: &RunArgs, force_paper: bool) -> anyhow::Result<()> {
    let rt = connect(&cli.home, &cli.config_path()).await?;
    let mode = rt.mode(force_paper);
    let metrics = PrometheusBuilder::new().install_recorder().ok();
    let shutdown = CancellationToken::new();
    let tid = rt.tid();
    let realtime = if rt.cfg.realtime.enabled {
        start_realtime(&rt, &tid, &shutdown).await
    } else {
        None
    };
    let kill = KillSwitch::new(rt.home.join(&rt.cfg.kill.file));
    let (bus, needle) = start_bus(&rt, kill.clone(), &shutdown).await;
    let enr = match (args.election, bus) {
        (false, _) => None,
        (true, None) => anyhow::bail!(
            "--election needs NATS at {} to publish county results",
            rt.cfg.bus.nats_url
        ),
        (true, Some(bus)) => {
            let enr_config = args
                .enr_config
                .clone()
                .unwrap_or_else(|| rt.home.join("config/enr_sources.toml"));
            let archive = rt.home.join(&rt.cfg.paths.data).join("enr");
            Some(election::start(&enr_config, &archive, bus, &shutdown)?)
        }
    };
    // The same switch (shared flag) is wired to NATS ops.kill and handed to the gateway.
    let mut engine = build_engine(&rt, mode, realtime, needle, Some(kill))?;

    // /healthz is up before first contact: under the per-account rate budget the bootstrap reads
    // can take minutes, and a health check that cannot connect gets the container restarted (watchdog)
    // before it ever finishes, which also burns the budget the live trader shares.
    let tick_ms = rt.cfg.live.tick_ms.max(50);
    let liveness = Arc::new(Mutex::new(Liveness {
        max_step_age_ns: i64::try_from(tick_ms).unwrap_or(500) * 20 * 1_000_000,
        bootstrapping: true,
        ..Liveness::default()
    }));
    let addr = rt.cfg.live.metrics_addr.parse().context("live.metrics_addr")?;
    let server = tokio::spawn(http::serve(addr, liveness.clone(), metrics, shutdown.clone()));
    tracing::info!(
        mode = mode.as_str(),
        "susq-live bootstrapping: first contact over REST"
    );
    engine.bootstrap().await;
    liveness.lock().bootstrapping = false;

    let status_every = i64::try_from(rt.cfg.live.status_interval_s).unwrap_or(60) * 1_000_000_000;
    let status_path = RunPaths::resolve(&rt.cfg, &rt.home, mode).status;
    let (mut next_status, mut seq) = (0i64, 0u64);
    let mut tick = tokio::time::interval(Duration::from_millis(tick_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let term = terminate_signal();
    tokio::pin!(term);
    tracing::info!(mode = engine.gateway().mode().as_str(), "susq-live running");
    loop {
        tokio::select! {
            () = &mut term => break,
            _ = tick.tick() => {
                engine.step().await;
                let now = rt.clock.now_ns();
                record_gauges(&engine);
                {
                    let mut l = liveness.lock();
                    l.last_step_ns = now;
                    l.now_ns = now;
                    l.body = json!({ "mode": engine.gateway().mode().as_str(), "killed": engine.gateway().kill().is_killed() });
                }
                if now >= next_status {
                    next_status = now + status_every;
                    seq += 1;
                    if let Err(e) = status::write(&status_path, &status::snapshot(&engine, now, seq)) {
                        tracing::warn!(error = %e, "writing status.json failed");
                    }
                }
            }
        }
    }
    tracing::info!("shutting down: cancelling all orders");
    engine.shutdown("SIGTERM").await;
    let now = rt.clock.now_ns();
    let _ = status::write(&status_path, &status::snapshot(&engine, now, seq + 1));
    shutdown.cancel();
    if let Some(enr) = enr {
        enr.shutdown().await;
    }
    let _ = server.await;
    drop(engine);
    rt.close().await;
    Ok(())
}

/// Loop-level gauges so `/metrics` always shows the engine's vital signs.
fn record_gauges(engine: &Engine) {
    let w = &engine.world;
    metrics::counter!("susq_engine_steps_total").increment(1);
    metrics::gauge!("susq_equity").set(w.equity);
    metrics::gauge!("susq_cash").set(w.cash);
    metrics::gauge!("susq_positions").set(w.positions.map().len() as f64);
    metrics::gauge!("susq_violations").set(w.violations.len() as f64);
    metrics::gauge!("susq_exchanges_with_live_orders").set(engine.live_exchanges().len() as f64);
    metrics::gauge!("susq_killed").set(if engine.gateway().kill().is_killed() {
        1.0
    } else {
        0.0
    });
    metrics::gauge!("susq_lease_held").set(if engine.gateway().lease().holds() {
        1.0
    } else {
        0.0
    });
    let needle_ok = w.needle.as_ref().is_some_and(|n| n.healthy);
    metrics::gauge!("susq_needle_healthy").set(if needle_ok { 1.0 } else { 0.0 });
    for h in w.health.values() {
        let v = match h.state.as_str() {
            "ok" => 2.0,
            "degraded" => 1.0,
            _ => 0.0,
        };
        metrics::gauge!("susq_health", "component" => h.name.clone()).set(v);
    }
}

/// Non-marketable price for a 1-contract smoke test: the best bid, kept one tick below the ask.
fn smoke_price(book: &susq_platform::OrderBook) -> Pips {
    let bid = book.best_bid.and_then(Pips::floor_from_prob).unwrap_or(Pips::MIN);
    match book
        .best_ask
        .and_then(Pips::ceil_from_prob)
        .and_then(|a| a.checked_add_ticks(-1))
    {
        Some(below_ask) if below_ask < bid => below_ask,
        _ => bid,
    }
}

/// `smoke`: place one contract and cancel it, through every gate. Prints a JSON result.
pub async fn smoke(cli: &Cli, exchange: &str) -> anyhow::Result<()> {
    let rt = connect(&cli.home, &cli.config_path()).await?;
    let mut engine = build_engine(&rt, rt.mode(false), None, None, None)?;
    engine.bootstrap().await;
    let ex = ExchangeId::new(exchange);
    let book = engine.platform().orderbook(&rt.tid(), &ex, 5).await?;
    let now = rt.clock.now();
    let intent_id = format!("smoke:{ex}:{}", now.timestamp());
    let spec = OrderSpec {
        intent_id: intent_id.clone(),
        strategy: "smoke".into(),
        exchange_id: ex.clone(),
        market_id: None,
        side: Side::Yes,
        action: Action::Buy,
        quantity: Qty(1),
        price: smoke_price(&book),
        ttl: Duration::from_secs(60),
        needle_derived: false,
        fair_yes: None,
    };
    engine
        .place(&OrderBundle::single(spec.clone()))
        .map_err(|b| anyhow::anyhow!("smoke order blocked: {b}"))?;
    engine.settle().await;
    let targets = engine
        .gateway()
        .oms()
        .lock()
        .request_cancel(|t| t.intent.intent_id == intent_id);
    let cancelled = engine.gateway().cancel_orders(&targets).await;
    let state = engine
        .gateway()
        .oms()
        .lock()
        .get(&intent_id)
        .map(|t| (t.state, t.order_id()));
    let ok = matches!(state, Some((OrderState::Cancelled | OrderState::Filled, Some(_))));
    println!(
        "{}",
        json!({
            "ok": ok,
            "mode": engine.gateway().mode().as_str(),
            "exchange": exchange,
            "price": spec.price.to_decimal_string(),
            "state": format!("{:?}", state.map(|s| s.0)),
            "order_id": state.and_then(|s| s.1).map(|o| o.0),
            "cancel_confirmed": cancelled,
            "at": from_ns(rt.clock.now_ns()).to_rfc3339(),
        })
    );
    engine.shutdown("smoke done").await;
    drop(engine);
    rt.close().await;
    anyhow::ensure!(ok, "smoke test did not place and cancel cleanly");
    Ok(())
}

/// `cancel-all`: needs the lease (refuses while another process trades).
pub async fn cancel_all(cli: &Cli, reason: &str) -> anyhow::Result<()> {
    let rt = connect(&cli.home, &cli.config_path()).await?;
    let mut engine = build_engine(&rt, rt.mode(false), None, None, None)?;
    engine.bootstrap().await;
    let resp = engine
        .gateway()
        .cancel_all(reason)
        .await
        .map_err(|b| anyhow::anyhow!("cancel-all blocked: {b}"))?;
    println!(
        "{}",
        json!({ "status": resp.http_status, "cancelled": resp.cancelled, "errors": resp.errors.len() })
    );
    let _ = engine.gateway().lease().release();
    drop(engine);
    rt.close().await;
    Ok(())
}

/// `lease`: print the lease on disk (no API key needed); `--paper` shows the paper-mode lease.
pub fn lease(cli: &Cli, paper: bool) -> anyhow::Result<()> {
    let cfg = susq_core::Config::load(&cli.config_path())?;
    let mode = if paper { Mode::Paper } else { Mode::Live };
    let path = RunPaths::resolve(&cfg, &cli.home, mode).lease;
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let lease: susq_contracts::v1::TraderLease = susq_contracts::from_json(&text)?;
            let now = susq_core::Clock::now_ns(&susq_core::SystemClock);
            let state = if lease.expires_at_ns > now {
                "held"
            } else {
                "expired"
            };
            println!(
                "{}",
                json!({ "path": path, "state": state, "lease": serde_json::from_str::<serde_json::Value>(&text)? })
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("{}", json!({ "path": path, "state": "none" }));
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_platform::{BookLevel, OrderBook};

    fn book(bid: Option<f64>, ask: Option<f64>) -> OrderBook {
        OrderBook {
            exchange_id: ExchangeId::new("1"),
            market_id: None,
            depth: 5,
            bids: bid
                .map(|p| {
                    vec![BookLevel {
                        price: p,
                        quantity: 1.0,
                    }]
                })
                .unwrap_or_default(),
            asks: vec![],
            best_bid: bid,
            best_ask: ask,
            spread: None,
        }
    }

    #[test]
    fn smoke_price_never_crosses() {
        assert_eq!(smoke_price(&book(Some(0.45), Some(0.5))).get(), 4500);
        assert_eq!(smoke_price(&book(Some(0.45), Some(0.455))).get(), 4500);
        assert_eq!(smoke_price(&book(None, Some(0.3))).get(), 50);
        assert_eq!(smoke_price(&book(None, None)).get(), 50);
    }
}
