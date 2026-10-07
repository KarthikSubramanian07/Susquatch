//! Election-night results pollers.
//!
//! Public results sites (CA SoS JSON, Clarity/Scytl ENR, Enhanced Voting with runtime discovery,
//! any static JSON via a config mapping)
//! are polled with jittered cadence and conditional GETs, normalized into cumulative
//! [`susq_contracts::v1::CountyResult`]s, validated, deduped, checked for monotonicity and
//! cross-source agreement, and emitted as `CountyResultsBatch`es on a `tokio::sync::mpsc` channel.
//! The `bus` crate publishes them to NATS `enr.county_results`; this crate only produces them.
//!
//! ```text
//!  source tasks (one per site)          pipeline task (single owner)
//!  ┌───────────────┐   PollOutput   ┌──────────────────────────────────────┐   CountyResultsBatch
//!  │ ca_sos        │ ─────────────► │ validate → tag → dedupe → monotonic  │ ─────────────────────►
//!  │ clarity:SC    │                │ → cross-source → header(seq) → chunk │   mpsc
//!  │ ga_sos (EV)   │                └──────────────────────────────────────┘
//!  │ static_json:* │
//!  └───────────────┘                         │ watch<EnrHealth> (disputed races, per-source health)
//! ```
//!
//! Modules: [`adapters`] (fetch + pure parsers), [`pipeline`] (the state machine),
//! [`service`] (tasks, backoff, health), [`archive`] (capture to `data/enr/`), [`config`],
//! [`fips`], [`http`], [`model`], [`units`] (county FIPS and House-district reporting units).

pub mod adapters;
pub mod archive;
pub mod config;
pub mod fips;
pub mod http;
pub mod model;
pub mod pipeline;
pub mod service;
pub mod units;

use config::EnrConfig;
use service::{ServiceConfig, SourceTask};

/// Build every enabled source from config, optionally restricted to `only` ids.
pub fn build_sources(cfg: &EnrConfig, only: &[String]) -> Result<Vec<SourceTask>, adapters::BuildError> {
    cfg.sources
        .iter()
        .filter(|s| s.enabled && (only.is_empty() || only.contains(&s.id)))
        .map(|s| {
            Ok(SourceTask {
                source: adapters::build_source(cfg, s)?,
                timing: cfg.timing(s),
            })
        })
        .collect()
}

/// Service settings derived from config.
pub fn service_config(cfg: &EnrConfig) -> ServiceConfig {
    ServiceConfig {
        pipeline: pipeline::PipelineConfig {
            consistency: cfg.consistency.clone(),
            confirm_corrections_after: cfg.defaults.confirm_corrections_after,
        },
        max_results_per_batch: cfg.defaults.max_results_per_batch,
        ..ServiceConfig::default()
    }
}
