//! `config/susq.toml`, shared with the Python bridge and scheduler, plus environment overrides.
//!
//! Sections the Python side does not use (`[bus]`, `[live]`, `[realtime]`, extra strategy knobs) have
//! Rust defaults, so the shared file stays minimal. Unknown keys are ignored so a Python-only addition
//! can never stop rust-live from starting. Secrets never live here (see [`crate::Secret`]).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::calendar::Calendar;

/// Paper (default) or live trading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Paper,
    Live,
}

impl Mode {
    /// Wire/status form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Paper => "paper",
            Mode::Live => "live",
        }
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub platform: PlatformCfg,
    pub calendar: Calendar,
    #[serde(default)]
    pub mode: ModeCfg,
    pub risk: RiskCfg,
    pub oms: OmsCfg,
    pub lease: LeaseCfg,
    pub kill: KillCfg,
    pub fair: FairCfg,
    #[serde(default)]
    pub strategies: StrategiesCfg,
    #[serde(default)]
    pub telemetry: TelemetryCfg,
    pub targets: TargetsCfg,
    pub needle: NeedleCfg,
    pub paths: PathsCfg,
    #[serde(default)]
    pub bus: BusCfg,
    #[serde(default)]
    pub live: LiveCfg,
    #[serde(default)]
    pub realtime: RealtimeCfg,
}

/// `[platform]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformCfg {
    pub base_url: String,
    pub alt_base_url: String,
    pub tournament_slug: String,
    pub request_timeout_ms: u64,
    pub initial_rps: f64,
    pub min_rps: f64,
    pub max_rps: f64,
    /// Share of the request budget reserved for order placement/cancel.
    pub order_budget_frac: f64,
}

/// `[mode]`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModeCfg {
    #[serde(default)]
    pub default: Mode,
}

/// `[risk]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskCfg {
    pub per_market_gross_frac: f64,
    pub total_gross_frac: f64,
    pub glitch_through_fair_pts: f64,
    pub glitch_max_frac: f64,
    pub loss_per_hour_frac: f64,
    pub reject_breaker_per_min: u32,
    pub max_order_notional_frac: f64,
    /// `[risk.clusters]`: cluster kind (`national`, `region`, `state`, `house`) → max gross fraction.
    #[serde(default)]
    pub clusters: BTreeMap<String, f64>,
    /// Bus validation errors per minute that trip the breaker.
    #[serde(default = "default_bus_error_breaker")]
    pub bus_error_breaker_per_min: u32,
}

fn default_bus_error_breaker() -> u32 {
    20
}

/// `[oms]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OmsCfg {
    pub default_ttl_s: u64,
    pub reconcile_interval_s: u64,
    pub idempotency_namespace: uuid::Uuid,
}

/// `[lease]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseCfg {
    pub path: PathBuf,
    pub ttl_s: u64,
    pub renew_s: u64,
}

/// `[kill]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KillCfg {
    pub file: PathBuf,
    pub go_live_file: PathBuf,
}

/// `[fair]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FairCfg {
    pub kalshi_weight: f64,
    pub sim_weight: f64,
    pub stale_after_s: u64,
    pub mapping_error_pts: f64,
}

/// `[strategies.*]`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StrategiesCfg {
    #[serde(default)]
    pub racer: RacerCfg,
    #[serde(default)]
    pub sniper: SniperCfg,
    #[serde(default)]
    pub mm: MmCfg,
    #[serde(default)]
    pub scenario: ScenarioCfg,
    #[serde(default)]
    pub en: EnCfg,
}

/// `[strategies.racer]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RacerCfg {
    pub enabled: bool,
    pub poll_interval_s: f64,
    /// Guaranteed-payoff margin per bundle share, in price units.
    pub min_edge: f64,
    pub max_bundle_notional_frac: f64,
    pub order_ttl_s: u64,
    /// Minimum seconds between two bundles on the same relationship, even if it stays violated.
    pub refire_cooldown_s: u64,
    /// Cap on notional already held across a relationship's legs, as a fraction of equity.
    pub max_relationship_notional_frac: f64,
}

impl Default for RacerCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_s: 3.0,
            min_edge: 0.01,
            max_bundle_notional_frac: 0.01,
            order_ttl_s: 20,
            refire_cooldown_s: 30,
            max_relationship_notional_frac: 0.03,
        }
    }
}

/// `[strategies.sniper]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SniperCfg {
    pub enabled: bool,
    pub min_edge_pts: f64,
    pub max_mistake_frac: f64,
    pub order_ttl_s: u64,
}

impl Default for SniperCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            min_edge_pts: 3.5,
            max_mistake_frac: 0.01,
            order_ttl_s: 30,
        }
    }
}

/// `[strategies.mm]` (disabled until the decision table says otherwise).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MmCfg {
    pub enabled: bool,
    pub quote_qty: i64,
    /// Ticks of skew per `inventory_unit` shares held (quotes lean against inventory).
    pub inventory_skew_ticks: f64,
    pub inventory_unit: i64,
    /// Ticks of skew at 100 % utilisation of the market's cluster cap.
    pub cluster_skew_ticks: f64,
    pub max_inventory: i64,
    /// A real-money move of this many points cancels every quote.
    pub cancel_move_pts: f64,
    pub order_ttl_s: u64,
}

impl Default for MmCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            quote_qty: 10,
            inventory_skew_ticks: 1.0,
            inventory_unit: 50,
            cluster_skew_ticks: 2.0,
            max_inventory: 500,
            cancel_move_pts: 2.0,
            order_ttl_s: 60,
        }
    }
}

/// `[strategies.scenario]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenarioCfg {
    pub enabled: bool,
    /// Fraction of the remaining gap to target worked per decision.
    pub tranche_frac: f64,
    pub min_tranche_qty: i64,
    pub order_ttl_s: u64,
}

impl Default for ScenarioCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            tranche_frac: 0.25,
            min_tranche_qty: 1,
            order_ttl_s: 120,
        }
    }
}

/// `[strategies.en]` (election night; enabled by config on the night).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnCfg {
    pub enabled: bool,
    /// Minimum `q - ask` in points.
    pub min_edge_pts: f64,
    /// ... and at least this many model standard errors.
    pub se_mult: f64,
    pub require_corroboration: bool,
    /// Sell an appreciated leg once the bid is within this many points of the posterior.
    pub roll_within_pts: f64,
    pub max_order_frac: f64,
    pub order_ttl_s: u64,
}

impl Default for EnCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            min_edge_pts: 8.0,
            se_mult: 3.0,
            require_corroboration: true,
            roll_within_pts: 2.0,
            max_order_frac: 0.02,
            order_ttl_s: 20,
        }
    }
}

/// `[telemetry]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryCfg {
    pub leaderboard_interval_s: u64,
    pub price_snapshot_interval_s: u64,
}

impl Default for TelemetryCfg {
    fn default() -> Self {
        Self {
            leaderboard_interval_s: 3600,
            price_snapshot_interval_s: 60,
        }
    }
}

/// `[targets]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetsCfg {
    pub dir: PathBuf,
    pub max_age_h: f64,
}

/// `[needle]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeedleCfg {
    pub heartbeat_timeout_s: u64,
    pub resume_after_healthy: u32,
}

/// `[paths]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathsCfg {
    pub journal: PathBuf,
    pub state: PathBuf,
    pub data: PathBuf,
    pub reports: PathBuf,
    pub status: PathBuf,
    pub market_map: PathBuf,
    /// Live fair values rewritten by the research scheduler (header + fair only).
    #[serde(default = "default_fair_live")]
    pub fair_live: PathBuf,
}

fn default_fair_live() -> PathBuf {
    PathBuf::from("state/fair_live.json")
}

/// `[bus]` (rust-only; defaults to the local NATS server).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusCfg {
    pub nats_url: String,
}

impl Default for BusCfg {
    fn default() -> Self {
        Self {
            nats_url: "nats://127.0.0.1:4222".into(),
        }
    }
}

/// `[live]` (rust-only process settings).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveCfg {
    pub status_interval_s: u64,
    /// Loopback only: `/healthz` and `/metrics`.
    pub metrics_addr: String,
    /// Books older than this are stale for decisions.
    pub book_max_age_s: u64,
    /// Main decision loop period.
    pub tick_ms: u64,
}

impl Default for LiveCfg {
    fn default() -> Self {
        Self {
            status_interval_s: 60,
            metrics_addr: "127.0.0.1:9464".into(),
            book_max_age_s: 10,
            tick_ms: 500,
        }
    }
}

/// `[realtime]` (rust-only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealtimeCfg {
    pub enabled: bool,
    /// Refresh the 3 h token this long before it expires.
    pub refresh_before_s: u64,
    pub heartbeat_s: u64,
    pub max_backoff_s: u64,
}

impl Default for RealtimeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_before_s: 900,
            heartbeat_s: 25,
            max_backoff_s: 30,
        }
    }
}

impl Config {
    /// Parse and validate TOML text.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load `path`, then apply environment overrides from the process environment.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        let mut cfg = Self::from_toml_str(&text)?;
        cfg.apply_env(|k| std::env::var(k).ok());
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply non-secret overrides: `SUSQ_BASE_URL`, `SUSQ_TOURNAMENT_SLUG`, `SUSQ_NATS_URL`,
    /// `SUSQ_METRICS_ADDR`. (`SUSQ_MODE` and `SUSQ_API_KEY` are read by the gateway/platform,
    /// never stored in the config.)
    pub fn apply_env(&mut self, lookup: impl Fn(&str) -> Option<String>) {
        if let Some(v) = lookup("SUSQ_BASE_URL").filter(|v| !v.is_empty()) {
            self.platform.base_url = v;
        }
        if let Some(v) = lookup("SUSQ_TOURNAMENT_SLUG").filter(|v| !v.is_empty()) {
            self.platform.tournament_slug = v;
        }
        if let Some(v) = lookup("SUSQ_NATS_URL").filter(|v| !v.is_empty()) {
            self.bus.nats_url = v;
        }
        if let Some(v) = lookup("SUSQ_METRICS_ADDR").filter(|v| !v.is_empty()) {
            self.live.metrics_addr = v;
        }
    }

    /// Reject values that would make the safety checks meaningless.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        self.calendar.validate().map_err(ConfigError::Invalid)?;
        let fracs = [
            ("risk.per_market_gross_frac", self.risk.per_market_gross_frac),
            ("risk.total_gross_frac", self.risk.total_gross_frac),
            ("risk.glitch_max_frac", self.risk.glitch_max_frac),
            ("risk.loss_per_hour_frac", self.risk.loss_per_hour_frac),
            ("risk.max_order_notional_frac", self.risk.max_order_notional_frac),
            ("platform.order_budget_frac", self.platform.order_budget_frac),
        ];
        for (name, v) in fracs
            .into_iter()
            .chain(self.risk.clusters.iter().map(|(k, v)| (k.as_str(), *v)))
        {
            if !(v.is_finite() && (0.0..=1.0).contains(&v)) {
                return bad(format!("{name} = {v} must be a fraction in [0, 1]"));
            }
        }
        let p = &self.platform;
        if !(p.min_rps > 0.0 && p.min_rps <= p.initial_rps && p.initial_rps <= p.max_rps) {
            return bad("platform rps must satisfy 0 < min <= initial <= max".into());
        }
        if self.lease.renew_s == 0 || self.lease.renew_s * 2 > self.lease.ttl_s {
            return bad("lease.renew_s must be > 0 and at most half of lease.ttl_s".into());
        }
        if self.oms.default_ttl_s == 0 {
            return bad("oms.default_ttl_s must be > 0".into());
        }
        if self.needle.heartbeat_timeout_s == 0 || self.needle.resume_after_healthy == 0 {
            return bad("needle timeouts must be > 0".into());
        }
        if self.targets.max_age_h.is_nan() || self.targets.max_age_h <= 0.0 {
            return bad("targets.max_age_h must be > 0".into());
        }
        Ok(())
    }

    /// Request timeout as a `Duration`.
    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.platform.request_timeout_ms)
    }

    /// Target file maximum age.
    pub fn targets_max_age(&self) -> Duration {
        Duration::from_secs_f64(self.targets.max_age_h * 3600.0)
    }

    /// Open instant (convenience).
    pub fn open_utc(&self) -> DateTime<Utc> {
        self.calendar.open_utc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_toml() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/susq.toml");
        std::fs::read_to_string(path).expect("config/susq.toml")
    }

    #[test]
    fn loads_the_shared_config() {
        let cfg = Config::from_toml_str(&shared_toml()).expect("valid shared config");
        assert_eq!(cfg.platform.tournament_slug, "midterm-elections");
        assert_eq!(cfg.mode.default, Mode::Paper);
        assert_eq!(
            cfg.calendar.last_order_utc.to_rfc3339(),
            "2026-11-04T16:55:00+00:00"
        );
        assert_eq!(cfg.risk.clusters.get("state"), Some(&0.10));
        assert_eq!(
            cfg.oms.idempotency_namespace.to_string(),
            "6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99"
        );
        assert!(!cfg.strategies.mm.enabled);
        assert!(cfg.strategies.racer.enabled);
        assert_eq!(cfg.needle.resume_after_healthy, 5);
        // Rust-only sections fall back to defaults.
        assert_eq!(cfg.bus.nats_url, "nats://127.0.0.1:4222");
        assert_eq!(cfg.paths.fair_live, PathBuf::from("state/fair_live.json"));
        assert!(cfg.live.metrics_addr.starts_with("127.0.0.1:"));
    }

    #[test]
    fn env_overrides() {
        let mut cfg = Config::from_toml_str(&shared_toml()).expect("valid");
        cfg.apply_env(|k| match k {
            "SUSQ_BASE_URL" => Some("http://127.0.0.1:9999/api/v1".into()),
            "SUSQ_NATS_URL" => Some("nats://10.0.0.1:4222".into()),
            _ => None,
        });
        assert_eq!(cfg.platform.base_url, "http://127.0.0.1:9999/api/v1");
        assert_eq!(cfg.bus.nats_url, "nats://10.0.0.1:4222");
    }

    #[test]
    fn rejects_unsafe_values() {
        let text = shared_toml().replace("total_gross_frac = 0.60", "total_gross_frac = 1.60");
        assert!(matches!(
            Config::from_toml_str(&text),
            Err(ConfigError::Invalid(_))
        ));
        let text = shared_toml().replace("renew_s = 5", "renew_s = 25");
        assert!(matches!(
            Config::from_toml_str(&text),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn tolerates_unknown_keys() {
        let text = format!("{}\n[python_only]\nfoo = 1\n", shared_toml());
        assert!(Config::from_toml_str(&text).is_ok());
    }

    #[test]
    fn load_reads_a_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("susq.toml");
        std::fs::write(&p, shared_toml()).expect("write");
        assert!(Config::load(&p).is_ok());
        assert!(matches!(
            Config::load(&dir.path().join("missing.toml")),
            Err(ConfigError::Io { .. })
        ));
    }
}
