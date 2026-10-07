//! `enr-capture`: poll the configured results sources and archive raw payloads plus normalized
//! batches under `data/enr/<source>/<date>/`. Run it during the CA SoS test windows (and on
//! election night as a side recorder) to build real replay data.
//!
//! ```text
//! cargo run -p susq-enr --bin enr-capture -- --config config/enr_sources.toml --duration-s 14400
//! ```

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use susq_enr::archive::Archive;
use susq_enr::config::EnrConfig;
use tokio::sync::mpsc;

#[derive(Debug, Parser)]
#[command(about = "Poll election-night results sources and archive what they return")]
struct Args {
    /// Sources + race map.
    #[arg(long, default_value = "config/enr_sources.toml")]
    config: PathBuf,
    /// Archive root (gitignored).
    #[arg(long, default_value = "data/enr")]
    out: PathBuf,
    /// Only these source ids (repeatable). Default: every enabled source.
    #[arg(long = "source")]
    sources: Vec<String>,
    /// Stop after this many seconds (default: run until Ctrl-C).
    #[arg(long)]
    duration_s: Option<u64>,
    /// Print a health line this often.
    #[arg(long, default_value_t = 30)]
    status_every_s: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,susq_enr=info".into()),
        )
        .init();
    let args = Args::parse();
    let mut cfg =
        EnrConfig::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    // Captures run deliberately during test windows: always at each source's own cadence.
    cfg.defaults.idle_until_utc = None;
    let sources = susq_enr::build_sources(&cfg, &args.sources)?;
    anyhow::ensure!(!sources.is_empty(), "no enabled sources match");
    tracing::info!(n = sources.len(), out = %args.out.display(), "capturing");

    let (tx, mut rx) = mpsc::channel(256);
    let handle = susq_enr::service::spawn(
        susq_enr::service_config(&cfg),
        sources,
        tx,
        Some(Archive::new(&args.out)),
    );
    let mut health = handle.health.clone();
    let deadline = args
        .duration_s
        .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut ticker = tokio::time::interval(Duration::from_secs(args.status_every_s.max(1)));
    let mut batches = 0u64;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            () = async { if let Some(d) = deadline { tokio::time::sleep_until(d).await } else { std::future::pending().await } } => break,
            b = rx.recv() => match b {
                Some(b) => {
                    batches += 1;
                    tracing::info!(seq = b.header.as_ref().map_or(0, |h| h.seq), results = b.results.len(), "batch");
                }
                None => break,
            },
            _ = ticker.tick() => {
                let h = health.borrow_and_update().clone();
                tracing::info!(batches, accepted = h.pipeline.accepted, duplicates = h.pipeline.duplicates,
                    rejected = h.pipeline.rejected, disputed = ?h.disputed_races, "status");
                for (source, s) in &h.sources {
                    if let Some(d) = &s.discovery {
                        tracing::info!(%source, phase = ?d.phase, date = %d.election_date,
                            election = ?d.election_id, production_verified = d.production_verified,
                            contests = ?d.contests, "discovery");
                    }
                }
            }
        }
    }
    let final_health = handle.health.borrow().clone();
    handle.shutdown().await;
    let summary = serde_json::to_string_pretty(&final_health)?;
    std::fs::create_dir_all(&args.out)?;
    std::fs::write(args.out.join("last_health.json"), &summary)?;
    println!("{summary}");
    Ok(())
}
