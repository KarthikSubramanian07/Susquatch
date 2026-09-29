//! Process setup shared by the commands: config, logging, metrics, journal, platform client,
//! tournament id.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::Context;
use susq_core::{Clock, Config, Mode, Secret, SystemClock, TournamentId};
use susq_gateway::{LimiterConfig, RateLimited, RateLimiter, resolve_mode};
use susq_journal::{Journal, JournalConfig};
use susq_platform::{PlatformAdapter, SuperMarket, SuperMarketConfig, Tournament};

use crate::wiring::PRODUCER;

/// A connected process.
pub struct Runtime {
    pub cfg: Config,
    pub home: std::path::PathBuf,
    pub clock: Arc<dyn Clock>,
    pub journal: Journal,
    pub journal_thread: std::thread::JoinHandle<std::io::Result<u64>>,
    pub platform: Arc<dyn PlatformAdapter>,
    pub limiter: Arc<RateLimiter>,
    pub tournament: Tournament,
}

impl Runtime {
    /// Tournament id.
    pub fn tid(&self) -> TournamentId {
        TournamentId::new(self.tournament.id.clone())
    }

    /// Mode from `SUSQ_MODE` and `ops/GO_LIVE`, unless forced to paper.
    pub fn mode(&self, force_paper: bool) -> Mode {
        if force_paper {
            return Mode::Paper;
        }
        resolve_mode(
            std::env::var("SUSQ_MODE").ok().as_deref(),
            &self.home.join(&self.cfg.kill.go_live_file),
        )
    }

    /// Flush and stop the journal writer.
    pub async fn close(self) {
        let _ = self.journal.flush().await;
        drop(self.platform);
        drop(self.journal);
        let _ = tokio::task::spawn_blocking(move || self.journal_thread.join()).await;
    }
}

/// Initialise tracing (JSON or human), filtered by `RUST_LOG` (default `info`, with the NATS
/// client's per-attempt reconnect chatter muted).
pub fn init_tracing(json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,async_nats=warn"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    let _ = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
}

/// Load config, open the journal, build the rate-limited platform client and resolve the
/// tournament (retrying: nothing trades without an explicit tournament id).
pub async fn connect(home: &Path, config_path: &Path) -> anyhow::Result<Runtime> {
    let cfg = Config::load(config_path).with_context(|| format!("loading {}", config_path.display()))?;
    let key = std::env::var("SUSQ_API_KEY").unwrap_or_default();
    anyhow::ensure!(
        !key.trim().is_empty(),
        "SUSQ_API_KEY is not set (paper mode also reads live books); put it in .env"
    );
    let key = Secret::new(key.trim());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (journal, journal_thread) = Journal::start(JournalConfig {
        dir: home.join(&cfg.paths.journal),
        producer: PRODUCER.into(),
        capacity: 4096,
        redact: vec![key.expose().to_owned()],
    })?;
    let sm = SuperMarket::new(
        SuperMarketConfig {
            base_url: cfg.platform.base_url.clone(),
            api_key: key,
            timeout: cfg.request_timeout(),
        },
        journal.clone(),
        clock.clone(),
    )?;
    let limiter = Arc::new(RateLimiter::new(LimiterConfig::from_cfg(&cfg.platform)));
    let platform: Arc<dyn PlatformAdapter> = Arc::new(RateLimited::new(Arc::new(sm), limiter.clone()));
    let tournament = resolve_tournament(platform.as_ref(), &cfg.platform.tournament_slug).await?;
    tracing::info!(slug = %tournament.slug, id = %tournament.id, members = tournament.member_count, "tournament resolved");
    Ok(Runtime {
        cfg,
        home: home.to_owned(),
        clock,
        journal,
        journal_thread,
        platform,
        limiter,
        tournament,
    })
}

async fn resolve_tournament(p: &dyn PlatformAdapter, slug: &str) -> anyhow::Result<Tournament> {
    let mut delay = Duration::from_secs(1);
    for attempt in 1..=8 {
        match p.tournament(slug).await {
            Ok(t) if !t.id.is_empty() => return Ok(t),
            Ok(_) => anyhow::bail!("tournament {slug} has no id"),
            Err(e) if e.retry_class() == susq_platform::RetryClass::Terminal => {
                anyhow::bail!("cannot resolve tournament {slug}: {e}")
            }
            Err(e) => {
                tracing::warn!(attempt, error = %e, "tournament lookup failed; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        }
    }
    anyhow::bail!("could not resolve tournament {slug} after retries")
}
