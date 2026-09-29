//! Runtime: one polling task per source feeding a single pipeline task that owns dedupe state,
//! stamps headers with a strictly increasing `seq`, and emits `CountyResultsBatch`es on an mpsc
//! channel. Health (per-source counters, disputed races) is published on a `watch` channel so the
//! election-night executor can refuse to act on disputed races or stale feeds.
//!
//! Failure behaviour: a failing source backs off exponentially (with jitter) and never affects
//! other sources; malformed payloads are counted and skipped; if the consumer drops the output
//! channel, every task stops.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Serialize;
use susq_contracts::v1::{CountyResultsBatch, Header, Producer};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::adapters::{PollOutput, ResultsSource, SourceError};
use crate::archive::Archive;
use crate::config::SourceTiming;
use crate::model::{PRODUCER, now_ns};
use crate::pipeline::{Pipeline, PipelineConfig, PipelineEvent, PipelineStats};

/// Service-wide settings.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub pipeline: PipelineConfig,
    pub max_results_per_batch: usize,
    /// Commit of the running build, stamped into every header.
    pub git_sha: String,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            pipeline: PipelineConfig::default(),
            max_results_per_batch: 500,
            git_sha: std::env::var("SUSQ_GIT_SHA").unwrap_or_else(|_| "unknown".into()),
        }
    }
}

/// Per-source health counters.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SourceHealth {
    pub polls: u64,
    pub ok_polls: u64,
    pub failed_polls: u64,
    pub consecutive_failures: u32,
    pub not_modified: u64,
    pub malformed_payloads: u64,
    pub unresolved_counties: u64,
    pub version_regressions: u64,
    /// Last successful poll (ns since epoch; 0 = never).
    pub last_ok_ns: i64,
    /// Last poll that produced at least one accepted result.
    pub last_new_data_ns: i64,
    pub last_error: Option<String>,
    /// Discovery state for sources that find their endpoints at runtime (Enhanced Voting),
    /// including whether the site flags the data as production or test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<crate::adapters::enhanced_voting::DiscoveryStatus>,
}

/// Snapshot published on the health channel after every poll.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EnrHealth {
    pub pipeline: PipelineStats,
    /// Races where sources disagree; executors must not trade them.
    pub disputed_races: BTreeSet<String>,
    pub dispute_details: BTreeMap<String, Vec<String>>,
    pub sources: BTreeMap<String, SourceHealth>,
    pub last_seq: u64,
    pub archive_errors: u64,
    pub updated_at_ns: i64,
}

enum Msg {
    Polled { source: String, output: PollOutput },
    Failed { source: String, error: SourceError },
}

/// A source plus how often to poll it.
pub struct SourceTask {
    pub source: Box<dyn ResultsSource>,
    pub timing: SourceTiming,
}

/// Handle to a running service.
pub struct EnrHandle {
    pub health: watch::Receiver<EnrHealth>,
    cancel: CancellationToken,
    tasks: JoinSet<()>,
}

impl EnrHandle {
    /// Stop polling and wait for every task to finish.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        while self.tasks.join_next().await.is_some() {}
    }

    /// Token that stops the service when cancelled.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Start polling. Batches go to `out`; raw payloads and batches are archived when `archive` is set.
pub fn spawn(
    cfg: ServiceConfig,
    sources: Vec<SourceTask>,
    out: mpsc::Sender<CountyResultsBatch>,
    archive: Option<Archive>,
) -> EnrHandle {
    let cancel = CancellationToken::new();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>(64);
    let mut initial = EnrHealth::default();
    for t in &sources {
        initial
            .sources
            .insert(t.source.id().to_owned(), SourceHealth::default());
    }
    let (health_tx, health_rx) = watch::channel(initial.clone());
    let mut tasks = JoinSet::new();
    for task in sources {
        tasks.spawn(poll_loop(task, msg_tx.clone(), cancel.clone()));
    }
    drop(msg_tx);
    let state = PipelineTask {
        pipeline: Pipeline::new(cfg.pipeline.clone()),
        cfg,
        seq: 0,
        health: initial,
        archive,
    };
    tasks.spawn(state.run(msg_rx, out, health_tx, cancel.clone()));
    EnrHandle {
        health: health_rx,
        cancel,
        tasks,
    }
}

async fn poll_loop(mut task: SourceTask, tx: mpsc::Sender<Msg>, cancel: CancellationToken) {
    let id = task.source.id().to_owned();
    let mut rng = SplitMix64::seeded(&id);
    let mut failures: u32 = 0;
    loop {
        let result = tokio::select! {
            () = cancel.cancelled() => return,
            r = task.source.poll() => r,
        };
        let msg = match result {
            Ok(output) => {
                failures = 0;
                Msg::Polled {
                    source: id.clone(),
                    output,
                }
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                tracing::warn!(source = %id, %error, "poll failed");
                Msg::Failed {
                    source: id.clone(),
                    error,
                }
            }
        };
        if tx.send(msg).await.is_err() {
            return;
        }
        let delay = next_delay(&task.timing, failures, rng.next_unit());
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Jittered interval; exponential backoff after consecutive failures, capped at `max_backoff`.
pub fn next_delay(t: &SourceTiming, failures: u32, unit: f64) -> Duration {
    let base = if failures == 0 {
        t.interval
    } else {
        let factor = 2f64.powi(failures.min(16) as i32);
        t.interval.mul_f64(factor).min(t.max_backoff)
    };
    let jitter = 1.0 + t.jitter_frac * (2.0 * unit - 1.0);
    base.mul_f64(jitter.max(0.05))
}

struct PipelineTask {
    pipeline: Pipeline,
    cfg: ServiceConfig,
    seq: u64,
    health: EnrHealth,
    archive: Option<Archive>,
}

impl PipelineTask {
    async fn run(
        mut self,
        mut rx: mpsc::Receiver<Msg>,
        out: mpsc::Sender<CountyResultsBatch>,
        health_tx: watch::Sender<EnrHealth>,
        cancel: CancellationToken,
    ) {
        loop {
            let msg = tokio::select! {
                () = cancel.cancelled() => return,
                m = rx.recv() => match m { Some(m) => m, None => return },
            };
            let batches = self.handle(msg);
            for b in batches {
                if out.send(b).await.is_err() {
                    tracing::info!("enr consumer dropped; stopping");
                    cancel.cancel();
                    return;
                }
            }
            health_tx.send_replace(self.health.clone());
        }
    }

    fn handle(&mut self, msg: Msg) -> Vec<CountyResultsBatch> {
        let now = now_ns();
        self.health.updated_at_ns = now;
        match msg {
            Msg::Failed { source, error } => {
                let h = self.health.sources.entry(source).or_default();
                h.polls += 1;
                h.failed_polls += 1;
                h.consecutive_failures = h.consecutive_failures.saturating_add(1);
                if matches!(error, SourceError::VersionRegression { .. }) {
                    h.version_regressions += 1;
                }
                if let SourceError::Malformed { .. } = error {
                    h.malformed_payloads += 1;
                }
                h.last_error = Some(error.to_string());
                Vec::new()
            }
            Msg::Polled { source, output } => {
                if let Some(archive) = self.archive.as_mut() {
                    for raw in &output.raw {
                        if let Err(e) = archive.write_raw(&source, raw, now) {
                            tracing::warn!(%source, error = %e, "archive raw failed");
                            self.health.archive_errors += 1;
                        }
                    }
                }
                {
                    let h = self.health.sources.entry(source.clone()).or_default();
                    h.polls += 1;
                    h.ok_polls += 1;
                    h.consecutive_failures = 0;
                    h.last_ok_ns = now;
                    h.not_modified += u64::from(output.not_modified);
                    h.malformed_payloads += u64::from(output.malformed_payloads);
                    h.unresolved_counties += u64::from(output.unresolved_counties);
                    h.last_error = output.partial_errors.last().cloned();
                    if let Some(d) = &output.discovery {
                        h.discovery = Some(d.as_ref().clone());
                    }
                }
                let outcome = self.pipeline.ingest(output.results);
                log_events(&source, &outcome.events);
                if let Some(archive) = &self.archive
                    && let Err(e) = archive.append_events(&source, &outcome.events, now)
                {
                    tracing::warn!(%source, error = %e, "archive events failed");
                    self.health.archive_errors += 1;
                }
                self.health.pipeline = self.pipeline.stats().clone();
                self.health.disputed_races = self.pipeline.disputed_races();
                self.health.dispute_details = self.pipeline.dispute_details();
                if outcome.accepted.is_empty() {
                    return Vec::new();
                }
                if let Some(h) = self.health.sources.get_mut(&source) {
                    h.last_new_data_ns = now;
                }
                let chunk = self.cfg.max_results_per_batch.max(1);
                let mut batches = Vec::new();
                for results in outcome.accepted.chunks(chunk) {
                    self.seq += 1;
                    let batch = CountyResultsBatch {
                        header: Some(Header {
                            schema_version: susq_contracts::SCHEMA_VERSION,
                            produced_at_ns: now_ns(),
                            producer: Some(Producer {
                                name: PRODUCER.into(),
                                git_sha: self.cfg.git_sha.clone(),
                            }),
                            seq: self.seq,
                        }),
                        results: results.to_vec(),
                    };
                    if let Some(archive) = &self.archive
                        && let Err(e) = archive.append_batch(&source, &batch, now)
                    {
                        tracing::warn!(%source, error = %e, "archive batch failed");
                        self.health.archive_errors += 1;
                    }
                    batches.push(batch);
                }
                self.health.last_seq = self.seq;
                batches
            }
        }
    }
}

fn log_events(source: &str, events: &[PipelineEvent]) {
    for e in events {
        match e {
            PipelineEvent::Rejected { reason, .. } => tracing::debug!(%source, %reason, "rejected result"),
            PipelineEvent::CorrectionHeld {
                race_id,
                county_fips,
                detail,
                ..
            } => {
                tracing::warn!(%source, %race_id, %county_fips, %detail, "count decreased; holding as correction");
            }
            PipelineEvent::CorrectionApplied {
                race_id, county_fips, ..
            } => {
                tracing::warn!(%source, %race_id, %county_fips, "correction confirmed and applied");
            }
            PipelineEvent::DisputeOpened {
                race_id,
                county_fips,
                sources,
                detail,
            } => {
                tracing::error!(%race_id, %county_fips, a = %sources.0, b = %sources.1, %detail, "sources disagree; race disputed");
            }
            PipelineEvent::DisputeCleared {
                race_id, county_fips, ..
            } => {
                tracing::info!(%race_id, %county_fips, "source disagreement resolved");
            }
        }
    }
}

/// Tiny deterministic PRNG for poll jitter (no need for a crypto RNG; seeded per source so
/// sources don't poll in lockstep).
struct SplitMix64(u64);

impl SplitMix64 {
    fn seeded(id: &str) -> Self {
        let mut s = now_ns() as u64;
        for b in id.bytes() {
            s = s.rotate_left(5) ^ u64::from(b);
        }
        Self(s)
    }

    fn next_unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> SourceTiming {
        SourceTiming {
            interval: Duration::from_secs(10),
            jitter_frac: 0.2,
            timeout: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }

    #[test]
    fn delay_is_jittered_within_bounds() {
        let t = timing();
        assert_eq!(next_delay(&t, 0, 0.5), Duration::from_secs(10));
        assert_eq!(next_delay(&t, 0, 0.0), Duration::from_secs(8));
        assert_eq!(next_delay(&t, 0, 1.0), Duration::from_secs(12));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let t = timing();
        assert_eq!(next_delay(&t, 1, 0.5), Duration::from_secs(20));
        assert_eq!(next_delay(&t, 2, 0.5), Duration::from_secs(40));
        assert_eq!(next_delay(&t, 9, 0.5), Duration::from_secs(60));
    }

    #[test]
    fn prng_is_in_unit_interval() {
        let mut r = SplitMix64::seeded("x");
        for _ in 0..1000 {
            let u = r.next_unit();
            assert!((0.0..1.0).contains(&u));
        }
    }
}
