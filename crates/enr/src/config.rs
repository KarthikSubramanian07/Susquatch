//! `config/enr_sources.toml`: which results sources to poll, how often, and how source contest
//! names map to our race ids.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
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
            if let SourceKind::EnhancedVoting { .. } = &s.kind {
                validate_enhanced_voting(s)?;
            }
            if let SourceKind::StaticJson {
                urls,
                version_url,
                version_path,
                ..
            } = &s.kind
            {
                let bad = |what: &str| Err(ConfigError::Invalid(format!("source {:?}: {what}", s.id)));
                match (version_url, version_path) {
                    (None, None) => {
                        if urls.iter().any(|u| u.contains("{version}")) {
                            return bad("{version} in urls needs version_url and version_path");
                        }
                    }
                    (Some(_), Some(p)) => {
                        if crate::adapters::json_map::Path::parse(p).is_err() {
                            return bad("version_path is not a path");
                        }
                        if urls.is_empty() || !urls.iter().all(|u| u.contains("{version}")) {
                            return bad("with version_url every url must contain {version}");
                        }
                    }
                    _ => return bad("version_url and version_path go together"),
                }
            }
        }
        for r in &self.races {
            if r.race_id.trim().is_empty() || r.contest.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "race mapping with empty contest or race_id".into(),
                ));
            }
            // A House race is reported as one district unit keyed off its race id (`crate::units`),
            // so the id must follow the race-id grammar exactly: `2026-HOUSE-CA-1` would match no
            // market and be modelled as county pieces.
            if crate::units::looks_like_house_race(&r.race_id)
                && crate::units::house_district(&r.race_id).is_none()
            {
                return Err(ConfigError::Invalid(format!(
                    "race {:?}: House race ids are <cycle>-HOUSE-<ST>-<two-digit district or AL>",
                    r.race_id
                )));
            }
            if r.source != "*" && !seen.contains(r.source.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "race {:?} names unknown source {:?}",
                    r.race_id, r.source
                )));
            }
        }
        if !(self.defaults.idle_interval_s.is_finite() && self.defaults.idle_interval_s >= 1.0) {
            return Err(ConfigError::Invalid(
                "defaults.idle_interval_s must be >= 1".into(),
            ));
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
            idle_until: self.defaults.idle_until_utc,
            idle_interval: Duration::from_secs_f64(self.defaults.idle_interval_s.max(1.0)),
        }
    }

    /// Which enabled sources can report each race id: `[[races]]` rows (wildcard rows apply to
    /// every enabled source) plus the `contests` of enhanced-voting sources. A race absent from
    /// the map has no feed at all.
    pub fn race_sources(&self) -> BTreeMap<String, BTreeSet<String>> {
        let enabled: Vec<&SourceConfig> = self.sources.iter().filter(|s| s.enabled).collect();
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for r in &self.races {
            for s in enabled.iter().filter(|s| r.source == "*" || r.source == s.id) {
                out.entry(r.race_id.clone()).or_default().insert(s.id.clone());
            }
        }
        for s in &enabled {
            if let SourceKind::EnhancedVoting { contests, .. } = &s.kind {
                for c in contests {
                    out.entry(c.race_id.clone()).or_default().insert(s.id.clone());
                }
            }
        }
        out
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

fn validate_enhanced_voting(s: &SourceConfig) -> Result<(), ConfigError> {
    let SourceKind::EnhancedVoting {
        base_url,
        jurisdiction,
        contests,
        discovery_interval_s,
        ..
    } = &s.kind
    else {
        return Ok(());
    };
    let bad = |what: &str| Err(ConfigError::Invalid(format!("source {:?}: {what}", s.id)));
    if reqwest::Url::parse(base_url).is_err() {
        return bad("base_url is not a URL");
    }
    if jurisdiction.trim().is_empty() {
        return bad("jurisdiction is empty");
    }
    if contests.is_empty() {
        return bad("contests is empty");
    }
    let mut names = std::collections::HashSet::new();
    for c in contests {
        if c.name.trim().is_empty() || c.race_id.trim().is_empty() {
            return bad("contest with empty name or race_id");
        }
        if !names.insert(fold_contest(&c.name)) {
            return bad(&format!("contest {:?} listed twice", c.name));
        }
    }
    if !discovery_interval_s.is_finite() || *discovery_interval_s < 1.0 {
        return bad("discovery_interval_s must be >= 1");
    }
    Ok(())
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
    /// Before this instant every source polls at `idle_interval_s` at most often: weeks of
    /// pre-election polling (the rust-live paper run, rehearsals) stay polite to state sites and
    /// still exercise every adapter. `None`: always the source's own interval. `enr-capture`
    /// clears it, since it only runs deliberately during test windows.
    pub idle_until_utc: Option<DateTime<Utc>>,
    /// Slowest pre-election cadence (seconds), see `idle_until_utc`.
    pub idle_interval_s: f64,
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
            idle_until_utc: None,
            idle_interval_s: 1800.0,
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
    /// Nevada Secretary of State `USandNV.xml`: county votes as `<County>Votes` attributes on
    /// each candidate (see [`crate::adapters::nv_sos`]).
    NvSos { urls: Vec<String> },
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
        /// Optional version document fetched before `urls` (e.g. Arizona's
        /// `election_<eid>_0.json`, whose `uploadId` names the current upload). The value at
        /// `version_path` replaces `{version}` in every url; a lower numeric version than the
        /// last one is refused as a regression, like Clarity's.
        #[serde(default)]
        version_url: Option<String>,
        #[serde(default)]
        version_path: Option<String>,
        mapping: Box<JsonMapping>,
    },
    /// A delimited results file (CSV/TSV, plain or zipped), mapped by column name with
    /// [`crate::adapters::csv::CsvMapping`] (e.g. North Carolina's `results_pct_*.zip`).
    Csv {
        urls: Vec<String>,
        mapping: Box<crate::adapters::csv::CsvMapping>,
    },
    /// Enhanced Voting (e.g. Georgia's results.sos.ga.gov): the election and its ballot items are
    /// discovered at runtime from the jurisdiction's election list, then each ballot item is
    /// mapped with [`JsonMapping`]. See [`crate::adapters::enhanced_voting`].
    EnhancedVoting {
        /// API root, e.g. `https://results.sos.ga.gov/results/public/api`.
        #[serde(default = "enhanced_voting_base_url")]
        base_url: String,
        /// Jurisdiction short name as it appears in the API path (`Georgia`).
        jurisdiction: String,
        /// Poll the election held on exactly this date (a quoted `"YYYY-MM-DD"` string).
        election_date: chrono::NaiveDate,
        /// Contests to follow, matched to ballot items by name (case/whitespace-insensitive).
        contests: Vec<ContestSpec>,
        /// How often to re-read the election list and ballot-item ids.
        #[serde(default = "enhanced_voting_discovery_s")]
        discovery_interval_s: f64,
        /// Withhold results unless the site marks the election `isProduction`.
        #[serde(default)]
        require_production: bool,
        mapping: Box<JsonMapping>,
    },
}

fn enhanced_voting_base_url() -> String {
    crate::adapters::enhanced_voting::DEFAULT_BASE_URL.to_owned()
}

fn enhanced_voting_discovery_s() -> f64 {
    600.0
}

/// One contest an [`SourceKind::EnhancedVoting`] source follows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContestSpec {
    /// Ballot-item name as the site prints it (e.g. `US Senate`).
    pub name: String,
    pub race_id: String,
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
    /// Poll no more often than `idle_interval` before this instant (`defaults.idle_until_utc`).
    pub idle_until: Option<DateTime<Utc>>,
    pub idle_interval: Duration,
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

    /// Add (or replace) one mapping.
    pub fn insert(&mut self, contest: &str, race_id: &str) {
        self.map.insert(fold_contest(contest), race_id.to_owned());
    }
}

/// Contest-name folding shared by every lookup: ASCII-lowercased, whitespace runs collapsed.
pub fn fold_contest(s: &str) -> String {
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
    fn race_sources_follow_enabled_sources_and_contests() {
        let cfg = EnrConfig::parse(SAMPLE).expect("parse");
        let rs = cfg.race_sources();
        let ids = |race: &str| -> Vec<&str> { rs[race].iter().map(String::as_str).collect() };
        assert_eq!(ids("2026-SEN-GA"), vec!["ca_sos", "clarity:GA"], "wildcard row");
        assert_eq!(ids("2026-SEN-GA-override"), vec!["clarity:GA"], "source row");
        // A disabled source feeds nothing.
        let off = SAMPLE.replace("poll_interval_s = 5", "poll_interval_s = 5\nenabled = false");
        let rs = EnrConfig::parse(&off).expect("parse").race_sources();
        assert_eq!(rs["2026-SEN-GA"].iter().collect::<Vec<_>>(), vec!["ca_sos"]);
        assert!(!rs.contains_key("2026-SEN-GA-override"));
        // Enhanced-voting contests count as that source's races.
        let rs = EnrConfig::parse(EV).expect("parse").race_sources();
        assert_eq!(rs["2026-SEN-GA"].iter().collect::<Vec<_>>(), vec!["ga_sos"]);
    }

    #[test]
    fn rejects_bad_configs() {
        let dup = SAMPLE.replace("clarity:GA\"\nstate", "ca_sos\"\nstate");
        assert!(EnrConfig::parse(&dup).is_err());
        let bad_state = SAMPLE.replace("state = \"GA\"", "state = \"Georgia\"");
        assert!(EnrConfig::parse(&bad_state).is_err());
        let unknown_src = SAMPLE.replace("source = \"clarity:GA\"", "source = \"nope\"");
        assert!(EnrConfig::parse(&unknown_src).is_err());
        for bad_house in ["2026-HOUSE-CA-1", "2026-HOUSE-CA-022", "2026-HOUSE-Cal-22"] {
            let text = SAMPLE.replace("2026-SEN-GA-override", bad_house);
            assert!(EnrConfig::parse(&text).is_err(), "{bad_house} must be refused");
        }
        let good_house = SAMPLE.replace("2026-SEN-GA-override", "2026-HOUSE-CA-01");
        assert!(EnrConfig::parse(&good_house).is_ok());
    }

    const EV: &str = r#"
[[sources]]
id = "ga_sos"
state = "GA"
kind = "enhanced_voting"
jurisdiction = "Georgia"
election_date = "2026-11-03"
contests = [{ name = "US Senate", race_id = "2026-SEN-GA" }]
discovery_interval_s = 600

[sources.mapping]
rows = "r[*]"
race = "$.n"
county = "c"
candidates = "o[*]"
candidate_name = "n"
candidate_votes = "v"
"#;

    #[test]
    fn enhanced_voting_parses_with_defaults() {
        let cfg = EnrConfig::parse(EV).expect("parse");
        let SourceKind::EnhancedVoting {
            base_url,
            election_date,
            contests,
            discovery_interval_s,
            require_production,
            ..
        } = &cfg.sources[0].kind
        else {
            panic!("enhanced_voting expected")
        };
        assert_eq!(base_url, crate::adapters::enhanced_voting::DEFAULT_BASE_URL);
        assert_eq!(election_date.to_string(), "2026-11-03");
        assert_eq!(contests[0].race_id, "2026-SEN-GA");
        assert!((discovery_interval_s - 600.0).abs() < f64::EPSILON);
        assert!(!require_production);
        assert!(crate::adapters::build_source(&cfg, &cfg.sources[0]).is_ok());
    }

    #[test]
    fn enhanced_voting_rejects_bad_settings() {
        for (from, to) in [
            ("\"2026-11-03\"", "\"Nov 3\""),
            (
                "contests = [{ name = \"US Senate\", race_id = \"2026-SEN-GA\" }]",
                "contests = []",
            ),
            (
                "contests = [{ name = \"US Senate\", race_id = \"2026-SEN-GA\" }]",
                "contests = [{ name = \"US Senate\", race_id = \"a\" }, { name = \"us  senate\", race_id = \"b\" }]",
            ),
            ("race_id = \"2026-SEN-GA\"", "race_id = \" \""),
            ("discovery_interval_s = 600", "discovery_interval_s = 0.5"),
            ("jurisdiction = \"Georgia\"", "jurisdiction = \"\""),
            (
                "jurisdiction = \"Georgia\"",
                "jurisdiction = \"Georgia\"\nbase_url = \"not a url\"",
            ),
        ] {
            let bad = EV.replace(from, to);
            assert_ne!(bad, EV, "replacement {from:?} must apply");
            assert!(EnrConfig::parse(&bad).is_err(), "{to:?} must be rejected");
        }
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
        let ga = cfg.sources.iter().find(|s| s.id == "ga_sos").expect("ga_sos");
        assert!(ga.enabled, "discovery is harmless before the election");
        let SourceKind::EnhancedVoting {
            election_date,
            contests,
            ..
        } = &ga.kind
        else {
            panic!("ga_sos must be enhanced_voting")
        };
        assert_eq!(election_date.to_string(), "2026-11-03");
        let ids: Vec<_> = contests.iter().map(|c| c.race_id.as_str()).collect();
        assert_eq!(ids, ["2026-SEN-GA", "2026-GOV-GA"]);
        // Every California district is mapped under both contest spellings (bulk and REST).
        for n in 1..=52 {
            let race = format!("2026-HOUSE-CA-{n:02}");
            assert_eq!(
                cfg.race_map("ca_sos")
                    .get(&format!("United States Representative District {n}")),
                Some(race.as_str())
            );
            assert_eq!(
                cfg.race_map("ca_sos_api")
                    .get(&format!("U.S. House of Representatives District {n}")),
                Some(race.as_str())
            );
        }
    }
}
