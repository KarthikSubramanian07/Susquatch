//! Assemble gateway, OMS, paper broker and engine from config. Used by the binary and by the
//! integration tests (which pass the mock platform and a manual clock).

use std::{path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use susq_bus::monitor::NeedleSnapshot;
use susq_core::{Clock, Config, Mode, TournamentId};
use susq_gateway::{Gateway, GatewayDeps, KillSwitch, Lease, LeaseConfig, PaperBroker, RetryPolicy};
use susq_journal::Journal;
use susq_oms::{Oms, OmsConfig};
use susq_platform::{PlatformAdapter, realtime::RealtimeEvent};
use tokio::sync::{mpsc, watch};

use crate::engine::{Engine, EngineDeps};

/// Producer / lease holder name of this process.
pub const PRODUCER: &str = "rust-live";

/// Inputs to [`build`].
pub struct BuildOpts {
    pub cfg: Config,
    pub home: PathBuf,
    pub mode: Mode,
    pub clock: Arc<dyn Clock>,
    /// Read endpoints (rate-limited platform in production, the mock in tests).
    pub platform: Arc<dyn PlatformAdapter>,
    pub journal: Journal,
    pub tournament: TournamentId,
    /// Paper account starting cash (the live tournament balance at start).
    pub paper_balance: f64,
    pub realtime: Option<mpsc::Receiver<RealtimeEvent>>,
    pub needle: Option<watch::Receiver<NeedleSnapshot>>,
    /// Lease identity (hostname, pid). Tests use distinct pids per "process".
    pub host: String,
    pub pid: u32,
    /// A kill switch created earlier (e.g. already wired to NATS `ops.kill`); a new one on the
    /// configured kill file otherwise.
    pub kill: Option<KillSwitch>,
}

/// The machine's host name for the lease (`HOSTNAME`, else `localhost`).
pub fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// Build the gateway (with its OMS) and the engine.
pub fn build(o: BuildOpts) -> anyhow::Result<Engine> {
    let cfg = &o.cfg;
    let (account, paper): (Arc<dyn PlatformAdapter>, Option<Arc<PaperBroker>>) = match o.mode {
        Mode::Live => (o.platform.clone(), None),
        Mode::Paper => {
            let broker = Arc::new(PaperBroker::new(
                o.platform.clone(),
                o.clock.clone(),
                o.tournament.clone(),
                &cfg.platform.tournament_slug,
                o.paper_balance,
            ));
            (broker.clone(), Some(broker))
        }
    };
    let oms = Oms::open(
        OmsConfig {
            namespace: cfg.oms.idempotency_namespace,
            sent_timeout_ns: 3 * i64::try_from(cfg.platform.request_timeout_ms).unwrap_or(10_000) * 1_000_000,
            max_resend_age_ns: 60 * 1_000_000_000,
        },
        &o.home.join(&cfg.paths.state).join(PRODUCER).join("intents.jsonl"),
        o.clock.now_ns(),
    )?;
    if oms.corrupt_lines() > 0 {
        tracing::warn!(lines = oms.corrupt_lines(), "intent log had unreadable lines");
    }
    let lease = Lease::new(
        LeaseConfig {
            path: o.home.join(&cfg.lease.path),
            ttl: Duration::from_secs(cfg.lease.ttl_s),
            holder: PRODUCER.into(),
            host: o.host,
            pid: o.pid,
        },
        o.clock.clone(),
    );
    let gateway = Gateway::new(GatewayDeps {
        writes: account.clone(),
        mode: o.mode,
        go_live_file: o.home.join(&cfg.kill.go_live_file),
        kill: o
            .kill
            .unwrap_or_else(|| KillSwitch::new(o.home.join(&cfg.kill.file))),
        lease,
        calendar: cfg.calendar,
        tournament: o.tournament.clone(),
        journal: o.journal,
        clock: o.clock.clone(),
        retry: RetryPolicy::default(),
        oms: Arc::new(Mutex::new(oms)),
    });
    Ok(Engine::new(EngineDeps {
        cfg: o.cfg,
        home: o.home,
        clock: o.clock,
        platform: o.platform,
        account,
        paper,
        gateway,
        tournament: o.tournament,
        realtime: o.realtime,
        needle: o.needle,
    }))
}
