//! `config/enr_sources.toml`: which results sources to poll, how often, and how source contest
//! names map to our race ids.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::adapters::json_map::JsonMapping;

/// Whole file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub consistency: ConsistencyConfig,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub races: Vec<RaceMapping>,
}

impl EnrConfig {
    /// Read and validate a config file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.display().to_string(), e))?;
        Self::parse(&text)
    }

    /// Parse and validate config text.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let cfg: EnrConfig = toml::from_str(text).map_err(|e| ConfigError::Toml(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut seen = std::collections::HashSet::new();
        for s in &self.sources {
            if !seen.insert(s.id.as_str()) {
                return Err(ConfigError::Invalid(format!("duplicate source id {:?}", s.id)));
            }
            if s.state.len() != 2 || !s.state.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(ConfigError::Invalid(format!(
                    "source {:?}: state must be a USPS code",
                    s.id
                )));
            }
            if s.id.contains('/') || s.id.contains("..") {
                return Err(ConfigError::Invalid(format!(
                    "source {:?}: id must be path-safe",
                    s.id
                )));
            }
        }
        for r in &self.races {
            if r.race_id.trim().is_empty() || r.contest.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "race mapping with empty contest or race_id".into(),
                ));
            }
            if r.source != "*" && !seen.contains(r.source.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "race {:?} names unknown source {:?}",
                    r.race_id, r.source
                )));
            }
        }
        if !(0.0..1.0).contains(&self.defaults.jitter_frac) {
            return Err(ConfigError::Invalid(
                "defaults.jitter_frac must be in [0, 1)".into(),
            ));
        }
        Ok(())
    }

    /// Effective timing for one source (source overrides, else defaults).
    pub fn timing(&self, s: &SourceConfig) -> SourceTiming {
        SourceTiming {
            interval: Duration::from_secs_f64(
                s.poll_interval_s
                    .unwrap_or(self.defaults.poll_interval_s)
                    .max(1.0),
            ),
            jitter_frac: s.jitter_frac.unwrap_or(self.defaults.jitter_frac),
            timeout: Duration::from_millis(s.timeout_ms.unwrap_or(self.defaults.timeout_ms)),
            max_backoff: Duration::from_secs_f64(self.defaults.max_backoff_s.max(1.0)),
        }
    }

    /// Contest-name → race-id lookup for one source.
    pub fn race_map(&self, source_id: &str) -> RaceMap {
        let mut map = HashMap::new();
        for r in self.races.iter().filter(|r| r.source == "*") {
            map.insert(fold_contest(&r.contest), r.race_id.clone());
        }
        // Source-specific rows win over wildcard rows.
        for r in self.races.iter().filter(|r| r.source == source_id) {
            map.insert(fold_contest(&r.contest), r.race_id.clone());
        }
        RaceMap { map }
    }
}

/// Defaults applied to every source.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    pub poll_interval_s: f64,
    pub jitter_frac: f64,
    pub timeout_ms: u64,
    pub max_backoff_s: f64,
    pub user_agent: String,
    /// A decrease must repeat this many consecutive polls before we accept it as a correction.
    pub confirm_corrections_after: u32,
    /// Split emitted batches so one bus message stays well under NATS' 1 MB default.
    pub max_results_per_batch: usize,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            poll_interval_s: 15.0,
            jitter_frac: 0.2,
            timeout_ms: 8_000,
            max_backoff_s: 120.0,
            user_agent: "susquatch-enr/0.1 (+https://github.com/KarthikSubramanian07/Susquatch)".into(),
            confirm_corrections_after: 2,
            max_results_per_batch: 500,
        }
    }
}

/// Cross-source agreement tolerances.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConsistencyConfig {
    /// Totals within this fraction are "the same count" and must agree on shares.
    pub total_tol_frac: f64,
    /// Floor on the vote tolerance, so tiny counties don't flap.
    pub abs_tol_votes: u64,
    /// Max two-party share difference (percentage points) between same-count sources.
    pub share_tol_pts: f64,
}

impl Default for ConsistencyConfig {
    fn default() -> Self {
        Self {
            total_tol_frac: 0.02,
            abs_tol_votes: 50,
            share_tol_pts: 1.0,
        }
    }
}

/// One results source.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    /// Stable id, used in `CountyResult.source` and archive paths (e.g. `ca_sos`, `clarity:GA`).
    pub id: String,
    /// USPS code of the state this source reports.
    pub state: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub poll_interval_s: Option<f64>,
    pub jitter_frac: Option<f64>,
    pub timeout_ms: Option<u64>,
    /// IANA zone for source timestamps printed without one (e.g. `America/Los_Angeles`).
    #[serde(default = "utc")]
    pub tz: chrono_tz::Tz,
    #[serde(flatten)]
    pub kind: SourceKind,
}

fn yes() -> bool {
    true
}

fn utc() -> chrono_tz::Tz {
    chrono_tz::Tz::UTC
}

/// Adapter-specific settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceKind {
    /// California Secretary of State JSON API: one URL per contest, county breakdown included.
    CaSos { urls: Vec<String> },
    /// Clarity/Scytl ENR: `base_url` is the election root, e.g.
    /// `https://results.enr.clarityelections.com/GA/122512`.
    Clarity {
        base_url: String,
        #[serde(default)]
        detail: crate::adapters::clarity::ClarityDetail,
    },
    /// Any static JSON document, mapped with [`JsonMapping`].
    StaticJson {
        urls: Vec<String>,
        mapping: Box<JsonMapping>,
    },
}

/// Maps a contest name as the source prints it to our race id.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RaceMapping {
    /// Source id, or `*` for every source.
    pub source: String,
    pub contest: String,
    pub race_id: String,
}

/// Resolved per-source timing.
#[derive(Debug, Clone, Copy)]
pub struct SourceTiming {
    pub interval: Duration,
    pub jitter_frac: f64,
    pub timeout: Duration,
    pub max_backoff: Duration,
}

/// Case- and whitespace-insensitive contest lookup.
#[derive(Debug, Clone, Default)]
pub struct RaceMap {
    map: HashMap<String, String>,
}

impl RaceMap {
    /// Build directly from (contest, race_id) pairs.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            map: pairs
                .into_iter()
                .map(|(c, r)| (fold_contest(c), r.to_owned()))
                .collect(),
        }
    }

    /// Race id for a source contest name, if we track it.
    pub fn get(&self, contest: &str) -> Option<&str> {
        self.map.get(&fold_contest(contest)).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

fn fold_contest(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("toml: {0}")]
    Toml(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[defaults]
poll_interval_s = 10

[[sources]]
id = "ca_sos"
state = "CA"
kind = "ca_sos"
urls = ["https://example.invalid/a"]

[[sources]]
id = "clarity:GA"
state = "GA"
kind = "clarity"
base_url = "https://example.invalid/GA/1"
poll_interval_s = 5

[[races]]
source = "*"
contest = "US  Senate"
race_id = "2026-SEN-GA"

[[races]]
source = "clarity:GA"
contest = "us senate"
race_id = "2026-SEN-GA-override"
"#;

    #[test]
    fn parses_and_resolves_race_map() {
        let cfg = EnrConfig::parse(SAMPLE).expect("parse");
        assert_eq!(cfg.sources.len(), 2);
        assert!(matches!(cfg.sources[1].kind, SourceKind::Clarity { .. }));
        assert_eq!(cfg.race_map("ca_sos").get("US Senate"), Some("2026-SEN-GA"));
        assert_eq!(
            cfg.race_map("clarity:GA").get("US SENATE"),
            Some("2026-SEN-GA-override")
        );
        assert_eq!(cfg.timing(&cfg.sources[0]).interval, Duration::from_secs(10));
        assert_eq!(cfg.timing(&cfg.sources[1]).interval, Duration::from_secs(5));
    }

    #[test]
    fn rejects_bad_configs() {
        let dup = SAMPLE.replace("clarity:GA\"\nstate", "ca_sos\"\nstate");
        assert!(EnrConfig::parse(&dup).is_err());
        let bad_state = SAMPLE.replace("state = \"GA\"", "state = \"Georgia\"");
        assert!(EnrConfig::parse(&bad_state).is_err());
        let unknown_src = SAMPLE.replace("source = \"clarity:GA\"", "source = \"nope\"");
        assert!(EnrConfig::parse(&unknown_src).is_err());
    }

    #[test]
    fn repo_config_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/enr_sources.toml");
        let cfg = EnrConfig::load(&path).expect("config/enr_sources.toml must parse");
        assert!(
            cfg.sources
                .iter()
                .any(|s| matches!(s.kind, SourceKind::CaSos { .. }))
        );
        assert!(
            cfg.sources
                .iter()
                .any(|s| matches!(s.kind, SourceKind::Clarity { .. }))
        );
    }
}
