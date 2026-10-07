//! Assemble gateway, OMS, paper broker and engine from config. Used by the binary and by the
//! integration tests (which pass the mock platform and a manual clock).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use susq_bus::monitor::NeedleSnapshot;
use susq_core::{Clock, Config, Mode, TournamentId};
use susq_gateway::{Gateway, GatewayDeps, KillSwitch, Lease, LeaseConfig, PaperBroker, RetryPolicy};
use susq_journal::Journal;
use susq_oms::{Oms, OmsConfig};
use susq_platform::PlatformAdapter;
use tokio::sync::watch;

use crate::{
    engine::{Engine, EngineDeps},
    enr_health::EnrFeed,
    realtime::RealtimeFeed,
};

/// Producer / lease holder name of this process.
pub const PRODUCER: &str = "rust-live";
/// Intents log directory (under `paths.state`) in paper mode.
const PAPER_INTENTS_DIR: &str = "rust-live-paper";

/// Every file this process writes under `state/` and `ops/state/`, resolved for one mode.
///
/// Paper mode never touches the live trader's files: a paper process started next to the live
/// bridge must not take the real lease the moment the bridge's lapses (which would silently stop
/// live trading), nor overwrite the live intents log or the status the dashboard shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPaths {
    pub mode: Mode,
    /// Single-trader lease (`<lease.path>`, or `<stem>.paper.<ext>` in paper mode).
    pub lease: PathBuf,
    /// Intents log (`<state>/rust-live/intents.jsonl`, or `<state>/rust-live-paper/` in paper).
    pub intents: PathBuf,
    /// Status snapshot (`<paths.status>`, or `<stem>.paper.<ext>` in paper mode).
    pub status: PathBuf,
}

/// `dir/name.ext` → `dir/name.paper.ext` (`dir/name` → `dir/name.paper`).
fn paper_variant(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let name = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}.paper.{ext}"),
        None => format!("{stem}.paper"),
    };
    path.with_file_name(name)
}

impl RunPaths {
    /// Resolve the paths of `mode` against `home`.
    pub fn resolve(cfg: &Config, home: &Path, mode: Mode) -> Self {
        let state = home.join(&cfg.paths.state);
        let (lease, intents, status) = match mode {
            Mode::Live => (
                home.join(&cfg.lease.path),
                state.join(PRODUCER),
                home.join(&cfg.paths.status),
            ),
            Mode::Paper => (
                paper_variant(&home.join(&cfg.lease.path)),
                state.join(PAPER_INTENTS_DIR),
                paper_variant(&home.join(&cfg.paths.status)),
            ),
        };
        Self {
            mode,
            lease,
            intents: intents.join("intents.jsonl"),
            status,
        }
    }

    /// Log where this process writes.
    pub fn announce(&self) {
        tracing::info!(
            mode = self.mode.as_str(),
            lease = %self.lease.display(),
            intents = %self.intents.display(),
            status = %self.status.display(),
            "state paths resolved"
        );
    }
}

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
    /// The realtime feed (per-market channels are managed by the engine).
    pub realtime: Option<RealtimeFeed>,
    pub needle: Option<watch::Receiver<NeedleSnapshot>>,
    /// The results pollers' health (`--election`): disputed and stale-feed gates for the executor.
    pub enr: Option<EnrFeed>,
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
    let paths = RunPaths::resolve(cfg, &o.home, o.mode);
    paths.announce();
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
        &paths.intents,
        o.clock.now_ns(),
    )?;
    if oms.corrupt_lines() > 0 {
        tracing::warn!(lines = oms.corrupt_lines(), "intent log had unreadable lines");
    }
    let lease = Lease::new(
        LeaseConfig {
            path: paths.lease,
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
        enr: o.enr,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let text =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/susq.toml"))
                .expect("shared config");
        Config::from_toml_str(&text).expect("valid config")
    }

    #[test]
    fn paper_mode_never_resolves_to_the_live_files() {
        let cfg = config();
        let home = Path::new("/srv/susq");
        let live = RunPaths::resolve(&cfg, home, Mode::Live);
        let paper = RunPaths::resolve(&cfg, home, Mode::Paper);
        assert_eq!(live.lease, home.join(&cfg.lease.path));
        assert_eq!(live.status, home.join(&cfg.paths.status));
        assert_eq!(live.intents, home.join("state/rust-live/intents.jsonl"));
        assert_eq!(paper.lease, home.join("ops/state/trader.paper.lease"));
        assert_eq!(paper.status, home.join("state/status.paper.json"));
        assert_eq!(paper.intents, home.join("state/rust-live-paper/intents.jsonl"));
        assert_ne!(paper.lease, live.lease);
        assert_ne!(paper.status, live.status);
        assert_ne!(paper.intents, live.intents);
    }

    #[test]
    fn paper_variant_keeps_the_extension() {
        assert_eq!(
            paper_variant(Path::new("a/b.json")),
            PathBuf::from("a/b.paper.json")
        );
        assert_eq!(
            paper_variant(Path::new("a/trader.lease")),
            PathBuf::from("a/trader.paper.lease")
        );
        assert_eq!(
            paper_variant(Path::new("a/noext")),
            PathBuf::from("a/noext.paper")
        );
    }
}
