//! Dress rehearsal harness (docs/RUNBOOK.md §2b) for a machine without the platform API key.
//!
//! `susq-live paper-run` needs `SUSQ_API_KEY` because paper mode reads live books. This harness
//! keeps every other piece real: the C++ needle on a real NATS server, rust-live's own bus client
//! and needle watchdog (`Bus::subscribe_needle` + `NeedleMonitor`, as `commands::run` wires them),
//! NATS `ops.kill`, and the full paper engine with the election-night executor armed. Only the
//! exchange is the in-process mock, seeded with one market per verified race rule in the market
//! map and a two-point book around the fair value of `state/fair_live.json`.
//!
//! Ignored by default (it needs a running stack). With nats + needle up and a replay publishing:
//!
//! ```text
//! SUSQ_REHEARSAL_NATS=nats://127.0.0.1:14222 SUSQ_REHEARSAL_HOME=<dir with state/market_map.json> \
//! SUSQ_REHEARSAL_SECS=600 cargo test -p susq-live --test rehearsal -- --ignored --nocapture
//! ```
//!
//! Optional: `SUSQ_REHEARSAL_OUT=<file>` writes the record as JSON; `SUSQ_REHEARSAL_RESTART_S=<n>`
//! rebuilds the engine (a `docker compose restart rust-live`) `n` seconds after an `ops.kill`, which
//! is how a bus kill is lifted. The test fails if an election-night order is placed while the needle
//! is unhealthy, if one is still working once the needle is unhealthy, or if no posterior arrives.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::json;
use susq_bus::{Bus, MonitorCfg, NeedleMonitor};
use susq_contracts::v1::MarketMap;
use susq_core::{Action, Clock, Config, Mode, Pips, Side, SystemClock, TournamentId};
use susq_gateway::KillSwitch;
use susq_journal::Journal;
use susq_live::{
    engine::Engine,
    wiring::{BuildOpts, build},
};
use susq_platform::mock::{MockConfig, MockPlatform};
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing_subscriber::{Layer, layer::SubscriberExt};

const TID: &str = "tid-rehearsal";
const SLUG: &str = "midterm-elections";
const STRATEGY: &str = "en";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The shipped config with only the rehearsal's changes: the executor armed (RUNBOOK §2b step 2),
/// no realtime (there is no exchange socket), and every other strategy off so the record shows the
/// executor alone.
fn config() -> Config {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join("config/susq.toml")).expect("shared config");
    let mut cfg = Config::from_toml_str(&text).expect("valid config");
    cfg.realtime.enabled = false;
    cfg.strategies.en.enabled = true;
    cfg.strategies.en.count_order_path = root.join("config/count_order.toml");
    cfg.strategies.racer.enabled = false;
    cfg.strategies.sniper.enabled = false;
    cfg.strategies.mm.enabled = false;
    cfg.strategies.scenario.enabled = false;
    cfg.strategies.farquote.enabled = false;
    cfg
}

/// `exchange id → fair p_yes` from a fair file (`{"fair": [{"exchangeId", "pYes"}]}`).
fn fair_values(path: &Path) -> HashMap<String, f64> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["fair"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some((f["exchangeId"].as_str()?.to_owned(), f["pYes"].as_f64()?)))
        .collect()
}

fn pips(p: f64) -> Pips {
    // Whole cents: the exchange's tick.
    let raw = ((p * 100.0).round() * 100.0).clamp(100.0, 9_900.0);
    Pips::new(raw as u32).expect("valid price")
}

/// One mock market per race rule, with 2,000 contracts a side one point either side of fair.
fn seed(mock: &MockPlatform, map: &MarketMap, fair: &HashMap<String, f64>) -> usize {
    let mut n = 0;
    for r in map
        .markets
        .iter()
        .filter(|r| r.kind == "race_party" && r.verified && !r.race_id.is_empty())
    {
        mock.add_market(
            r.market_id.as_str(),
            &r.title,
            &[(r.exchange_id.as_str(), r.party.as_str())],
        );
        let p = fair.get(&r.exchange_id).copied().unwrap_or(0.5);
        mock.seed_order(
            "crowd",
            r.exchange_id.as_str(),
            Side::Yes,
            Action::Buy,
            2_000,
            pips(p - 0.01),
        );
        mock.seed_order(
            "crowd",
            r.exchange_id.as_str(),
            Side::Yes,
            Action::Sell,
            2_000,
            pips(p + 0.01),
        );
        n += 1;
    }
    n
}

/// Everything the executor and the order path said, by reason (exchange ids and numbers folded).
#[derive(Default)]
struct Reasons(Mutex<BTreeMap<String, u64>>);

impl Reasons {
    fn fold(text: &str) -> String {
        let text = text
            .strip_prefix("en ")
            .map_or(text, |t| t.split_once(": ").map_or(t, |(_, r)| r));
        let mut out = String::with_capacity(text.len());
        let mut in_num = false;
        for c in text.chars() {
            if c.is_ascii_digit() || (in_num && c == '.') {
                if !in_num {
                    out.push('#');
                }
                in_num = true;
            } else {
                in_num = false;
                out.push(c);
            }
        }
        out
    }
}

#[derive(Default)]
struct Fields {
    strategy: Option<String>,
    note: Option<String>,
    reason: Option<String>,
    message: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &value);
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let v = format!("{value:?}");
        let v = v.trim_matches('"').to_owned();
        match field.name() {
            "strategy" => self.strategy = Some(v),
            "note" => self.note = Some(v),
            "reason" => self.reason = Some(v),
            "message" => self.message = Some(v),
            _ => {}
        }
    }
}

struct Capture(Arc<Reasons>);

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut f = Fields::default();
        event.record(&mut f);
        if f.strategy.as_deref() != Some(STRATEGY) && f.strategy.as_deref() != Some("Some(\"en\")") {
            return;
        }
        let line = match (f.note, f.reason, f.message.as_deref()) {
            (Some(note), _, _) => format!("note: {}", Reasons::fold(&note)),
            (_, Some(reason), Some("order blocked")) => format!("blocked: {}", Reasons::fold(&reason)),
            _ => return,
        };
        *self.0.0.lock().entry(line).or_default() += 1;
    }
}

fn en_intents(engine: &Engine) -> Vec<(String, bool)> {
    engine
        .gateway()
        .oms()
        .lock()
        .all()
        .filter(|t| t.intent.strategy == STRATEGY)
        .map(|t| (t.intent.intent_id.clone(), t.state.is_live()))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn engine(
    cfg: &Config,
    home: &Path,
    clock: &Arc<dyn Clock>,
    mock: &Arc<MockPlatform>,
    monitor: &NeedleMonitor,
    kill: &KillSwitch,
    pid: u32,
) -> Engine {
    build(BuildOpts {
        cfg: cfg.clone(),
        home: home.to_owned(),
        mode: Mode::Paper,
        clock: clock.clone(),
        platform: mock.clone(),
        journal: Journal::discard(),
        tournament: TournamentId::new(TID),
        paper_balance: 100_000.0,
        realtime: None,
        needle: Some(monitor.subscribe()),
        enr: None,
        host: "rehearsal".into(),
        pid,
        kill: Some(kill.clone()),
    })
    .expect("engine")
}

/// A bus kill (`ops.kill`) sets a sticky flag; the returned switch is what the engine checks.
async fn kill_switch(bus: &Bus, file: PathBuf, cancel: &CancellationToken) -> KillSwitch {
    let kill = KillSwitch::new(file);
    if let Ok(mut kills) = bus.subscribe_kill().await {
        let (k, cancel) = (kill.clone(), cancel.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    m = kills.next() => match m {
                        Some(m) => k.trigger(format!("ops.kill from {}: {}", m.issuer, m.reason)),
                        None => break,
                    },
                }
            }
        });
    }
    kill
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs nats + needle + a replay publishing (docs/RUNBOOK.md §2b)"]
async fn rehearsal_against_a_live_needle() {
    let Some(nats) = env("SUSQ_REHEARSAL_NATS") else {
        eprintln!("SUSQ_REHEARSAL_NATS not set: nothing to rehearse against");
        return;
    };
    let src = PathBuf::from(env("SUSQ_REHEARSAL_HOME").expect("SUSQ_REHEARSAL_HOME"));
    let secs: u64 = env("SUSQ_REHEARSAL_SECS").map_or(300, |s| s.parse().expect("SUSQ_REHEARSAL_SECS"));
    let restart_s: Option<u64> =
        env("SUSQ_REHEARSAL_RESTART_S").map(|s| s.parse().expect("SUSQ_REHEARSAL_RESTART_S"));

    let reasons = Arc::new(Reasons::default());
    // What `RUST_LOG=info,susq_live::en=debug` shows on the VM: the executor's notes and the
    // engine's `order blocked` lines.
    let filter = tracing_subscriber::EnvFilter::new("warn,susq_live::engine=info,susq_live::en=debug");
    let subscriber = tracing_subscriber::registry().with(Capture(reasons.clone()).with_filter(filter));
    let _guard = tracing::subscriber::set_default(subscriber);

    let cfg = config();
    let home = tempfile::tempdir().expect("tmp");
    let map_text = std::fs::read_to_string(src.join(&cfg.paths.market_map)).expect("market map");
    let map: MarketMap = susq_contracts::from_json(&map_text).expect("market map json");
    let map_path = home.path().join(&cfg.paths.market_map);
    std::fs::create_dir_all(map_path.parent().expect("dir")).expect("mkdir");
    std::fs::write(&map_path, &map_text).expect("write map");
    let known: HashSet<String> = map
        .markets
        .iter()
        .map(|r| r.race_id.clone())
        .filter(|r| !r.is_empty())
        .collect();

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mock = Arc::new(MockPlatform::new(
        clock.clone(),
        MockConfig {
            tournament_id: TID.into(),
            slug: SLUG.into(),
            starting_balance: 100_000.0,
            member_count: 1200,
            page_size: 200,
            collateral: true,
        },
    ));
    let seeded = seed(&mock, &map, &fair_values(&src.join("state/fair_live.json")));

    let cancel = CancellationToken::new();
    let bus = Bus::connect(&nats, "rust-live-rehearsal", "local")
        .await
        .expect("NATS");
    let messages = bus.subscribe_needle().await.expect("needle subjects");
    let monitor = NeedleMonitor::spawn(
        messages,
        known.clone(),
        MonitorCfg::from_config(&cfg.needle),
        clock.clone(),
        cancel.clone(),
    );
    let kill_file = home.path().join(&cfg.kill.file);
    let mut kill = kill_switch(&bus, kill_file.clone(), &cancel).await;
    let mut pid = 1;
    let mut eng = engine(&cfg, home.path(), &clock, &mock, &monitor, &kill, pid);
    eng.bootstrap().await;
    eprintln!(
        "rehearsal: {seeded} mock markets on {} known races; running {secs} s",
        known.len()
    );

    let tick = Duration::from_millis(cfg.live.tick_ms.max(50));
    let start = Instant::now();
    let mut seen: HashSet<String> = HashSet::new();
    let mut posterior_updates = 0u64;
    let mut last_post: HashMap<String, (u32, i64)> = HashMap::new();
    let mut races_with_posteriors: HashSet<String> = HashSet::new();
    let (mut healthy_was, mut outages, mut down_s) = (None::<bool>, 0u32, 0.0f64);
    let (mut placed_while_down, mut live_while_down) = (Vec::new(), Vec::new());
    let mut killed_at: Option<Instant> = None;
    let mut kills = 0u32;
    let mut placed_while_killed = 0usize;
    let mut events = Vec::new();

    while start.elapsed() < Duration::from_secs(secs) {
        let t0 = Instant::now();
        eng.step().await;
        eng.settle().await;
        let healthy = eng.world.needle.as_ref().is_some_and(|n| n.healthy);
        let is_killed = eng.gateway().kill().is_killed();
        if let Some(n) = eng.world.needle.as_ref() {
            for (race, p) in &n.posteriors {
                let key = (p.n_batches, i64::from(p.n_batches) ^ (p.p_dem * 1e6) as i64);
                if last_post.insert(race.clone(), key) != Some(key) {
                    posterior_updates += 1;
                    races_with_posteriors.insert(race.clone());
                }
            }
        }
        for (id, live) in en_intents(&eng) {
            let new = seen.insert(id.clone());
            if new && !healthy {
                placed_while_down.push(id.clone());
            }
            if new && is_killed {
                placed_while_killed += 1;
            }
            if live && !healthy && healthy_was == Some(false) {
                live_while_down.push(id);
            }
        }
        if healthy_was == Some(true) && !healthy {
            outages += 1;
            let why = eng
                .world
                .health
                .get("needle")
                .map(|h| h.detail.clone())
                .unwrap_or_default();
            eprintln!(
                "t+{:>5.0}s needle UNHEALTHY: {why}",
                start.elapsed().as_secs_f64()
            );
        }
        if healthy_was == Some(false) && healthy {
            eprintln!("t+{:>5.0}s needle healthy again", start.elapsed().as_secs_f64());
        }
        if !healthy && healthy_was.is_some() {
            down_s += tick.as_secs_f64().max(t0.elapsed().as_secs_f64());
        }
        healthy_was = Some(healthy);
        events.extend(eng.world.events.drain(..));
        if is_killed && killed_at.is_none() {
            kills += 1;
            killed_at = Some(Instant::now());
            eprintln!(
                "t+{:>5.0}s KILLED: {}",
                start.elapsed().as_secs_f64(),
                kill.reason().unwrap_or_default()
            );
        }
        if let (Some(at), Some(after)) = (killed_at, restart_s)
            && at.elapsed() >= Duration::from_secs(after)
        {
            // Un-kill a bus kill the way the VM does: restart the process (a new switch, a new engine).
            eng.shutdown("rehearsal restart").await;
            pid += 1;
            kill = kill_switch(&bus, kill_file.clone(), &cancel).await;
            eng = engine(&cfg, home.path(), &clock, &mock, &monitor, &kill, pid);
            eng.bootstrap().await;
            killed_at = None;
            eprintln!("t+{:>5.0}s restarted (un-kill)", start.elapsed().as_secs_f64());
        }
        tokio::time::sleep(tick.saturating_sub(t0.elapsed())).await;
    }

    let positions = eng.account().positions(SLUG).await.expect("positions");
    let held: Vec<_> = positions
        .positions
        .iter()
        .filter(|p| p.quantity.abs() > 0.0)
        .map(|p| json!({"exchange": p.exchange_id.as_str(), "qty": p.quantity}))
        .collect();
    let record = json!({
        "seconds": secs,
        "known_races": known.len(),
        "mock_markets": seeded,
        "posterior_updates": posterior_updates,
        "races_with_posteriors": races_with_posteriors.len(),
        "needle_outages": outages,
        "needle_down_s": down_s.round(),
        "en_intents": seen.len(),
        "en_intents_placed_while_needle_down": placed_while_down.len(),
        "en_intents_working_while_needle_down": live_while_down.len(),
        "kills": kills,
        "en_intents_placed_while_killed": placed_while_killed,
        "paper_positions": held,
        "reasons": *reasons.0.lock(),
        "events": events.iter().filter(|e| !e.starts_with("order outcome")).take(200).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&record).expect("json");
    eprintln!("{text}");
    if let Some(out) = env("SUSQ_REHEARSAL_OUT") {
        std::fs::write(out, &text).expect("write record");
    }
    cancel.cancel();
    eng.shutdown("rehearsal end").await;

    assert!(posterior_updates > 0, "no posterior reached the executor");
    assert!(
        placed_while_down.is_empty(),
        "election-night orders placed while the needle was down: {placed_while_down:?}"
    );
    assert!(
        live_while_down.is_empty(),
        "election-night orders still working while the needle was down: {live_while_down:?}"
    );
    assert_eq!(
        placed_while_killed, 0,
        "election-night orders placed while killed"
    );
}
