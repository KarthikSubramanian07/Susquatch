//! `--election`: run the results pollers (`susq-enr`) inside rust-live and publish every
//! `CountyResultsBatch` they produce on NATS `enr.county_results` for the needle.
//!
//! The bus re-stamps each batch with rust-live's header and a per-process strictly increasing
//! `seq`, which is the sequence the needle gap-checks. A failed publish still consumes a seq, so a
//! lost batch shows up downstream as a gap (and the needle side fails safe) rather than silently.
//! Raw payloads and batches are archived under `data/enr/` for replays, like `enr-capture`.

use std::{path::Path, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use susq_bus::Bus;
use susq_contracts::v1::{CountyResult, CountyResultsBatch};
use susq_enr::{archive::Archive, config::EnrConfig, service::EnrHandle};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::enr_health::EnrFeed;

/// A running election mode: the pollers plus what the engine reads from them.
pub struct Election {
    pub handle: EnrHandle,
    /// Health channel and race → source map for the executor's gates.
    pub feed: EnrFeed,
}

/// Where county results go (NATS in production, a recorder in tests).
#[async_trait]
pub trait CountySink: Send + Sync + 'static {
    /// Publish one batch's results; returns the published seq.
    async fn publish(&self, results: Vec<CountyResult>) -> Result<u64, String>;
}

#[async_trait]
impl CountySink for Bus {
    async fn publish(&self, results: Vec<CountyResult>) -> Result<u64, String> {
        self.publish_county_results(results)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Forward batches from the pollers to `sink` until the channel closes or `cancel` fires.
/// Returns `(published, failed)` batch counts.
pub async fn forward(
    mut rx: mpsc::Receiver<CountyResultsBatch>,
    sink: Arc<dyn CountySink>,
    cancel: CancellationToken,
) -> (u64, u64) {
    let (mut published, mut failed) = (0u64, 0u64);
    loop {
        let batch = tokio::select! {
            () = cancel.cancelled() => break,
            b = rx.recv() => match b {
                Some(b) => b,
                None => break,
            },
        };
        let n = batch.results.len();
        match sink.publish(batch.results).await {
            Ok(seq) => {
                published += 1;
                metrics::counter!("susq_enr_batches_published_total").increment(1);
                tracing::debug!(seq, results = n, "published county results");
            }
            Err(e) => {
                failed += 1;
                metrics::counter!("susq_enr_publish_errors_total").increment(1);
                tracing::warn!(error = %e, results = n, "publishing county results failed (the needle will see a seq gap)");
            }
        }
    }
    (published, failed)
}

/// Load the source config, start the pollers and the forwarder. Refuses to start with no
/// enabled sources (election mode that polls nothing is an operator mistake worth failing on).
pub fn start(
    enr_config: &Path,
    archive_root: &Path,
    sink: Arc<dyn CountySink>,
    cancel: &CancellationToken,
) -> anyhow::Result<Election> {
    let cfg = EnrConfig::load(enr_config).with_context(|| format!("loading {}", enr_config.display()))?;
    let sources = susq_enr::build_sources(&cfg, &[])?;
    anyhow::ensure!(
        !sources.is_empty(),
        "{}: no enabled results sources",
        enr_config.display()
    );
    tracing::info!(sources = sources.len(), "election mode: polling results sites");
    let (tx, rx) = mpsc::channel(256);
    let handle = susq_enr::service::spawn(
        susq_enr::service_config(&cfg),
        sources,
        tx,
        Some(Archive::new(archive_root)),
    );
    tokio::spawn(forward(rx, sink, cancel.clone()));
    let feed = EnrFeed {
        health: handle.health.clone(),
        race_sources: cfg.race_sources(),
    };
    Ok(Election { handle, feed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Recorder {
        got: Mutex<Vec<usize>>,
        fail_every_other: bool,
    }

    #[async_trait]
    impl CountySink for Recorder {
        async fn publish(&self, results: Vec<CountyResult>) -> Result<u64, String> {
            let mut got = self.got.lock();
            got.push(results.len());
            if self.fail_every_other && got.len().is_multiple_of(2) {
                Err("nats down".into())
            } else {
                Ok(got.len() as u64)
            }
        }
    }

    fn batch(n: usize) -> CountyResultsBatch {
        CountyResultsBatch {
            header: None,
            results: vec![CountyResult::default(); n],
        }
    }

    #[tokio::test]
    async fn forwards_every_batch_until_the_pollers_stop() {
        let (tx, rx) = mpsc::channel(8);
        let sink = Arc::new(Recorder::default());
        let task = tokio::spawn(forward(rx, sink.clone(), CancellationToken::new()));
        for n in [3, 1, 2] {
            tx.send(batch(n)).await.expect("send");
        }
        drop(tx);
        assert_eq!(task.await.expect("join"), (3, 0));
        assert_eq!(*sink.got.lock(), vec![3, 1, 2]);
    }

    #[tokio::test]
    async fn publish_failures_are_counted_and_do_not_stop_forwarding() {
        let (tx, rx) = mpsc::channel(8);
        let sink = Arc::new(Recorder {
            fail_every_other: true,
            ..Recorder::default()
        });
        let task = tokio::spawn(forward(rx, sink, CancellationToken::new()));
        for _ in 0..4 {
            tx.send(batch(1)).await.expect("send");
        }
        drop(tx);
        assert_eq!(task.await.expect("join"), (2, 2));
    }

    #[tokio::test]
    async fn cancel_stops_forwarding() {
        let (_tx, rx) = mpsc::channel::<CountyResultsBatch>(8);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(forward(rx, Arc::new(Recorder::default()), cancel.clone()));
        cancel.cancel();
        assert_eq!(task.await.expect("join"), (0, 0));
    }

    #[test]
    fn the_shipped_source_config_builds() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/enr_sources.toml");
        let cfg = EnrConfig::load(&path).expect("config/enr_sources.toml parses");
        let sources = susq_enr::build_sources(&cfg, &[]).expect("sources build");
        assert!(!sources.is_empty(), "at least one enabled source");
        // The MVP races each have a feed (CA SoS bulk, GA enhanced voting).
        let rs = cfg.race_sources();
        assert!(rs["2026-GOV-CA"].contains("ca_sos"));
        assert!(rs["2026-SEN-GA"].contains("ga_sos"));
        assert!(rs["2026-GOV-GA"].contains("ga_sos"));
    }

    #[tokio::test]
    async fn refuses_a_config_without_sources() {
        let dir = tempfile::tempdir().expect("tmp");
        let missing = dir.path().join("nope.toml");
        let sink: Arc<dyn CountySink> = Arc::new(Recorder::default());
        assert!(start(&missing, dir.path(), sink, &CancellationToken::new()).is_err());
    }
}
