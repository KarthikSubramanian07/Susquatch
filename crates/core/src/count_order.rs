//! The pre-registered count-order playbook (`config/count_order.toml`, EXP-006).
//!
//! Per state, the expected partial-count bias by hour after poll close: `drift` = (reported D minus
//! R margin at the hour) minus (final margin), in points, with an uncertainty band. Negative drift
//! means the partial count is more Republican than the final, so the count still to come favours
//! the Democrat. Points sit at +1, +3, +6, +12 hours and the lock; [`Profile::at`] interpolates
//! linearly between them, holds the first value before +1, goes linearly to zero between the lock
//! and `final_h`, and is zero after. Nothing is registered before poll close.
//!
//! A drift has a registered **direction** only when `|drift| > band` ([`Registered::favours`]).
//! Race-level overrides (`[states.XX.races."<pattern>"]`) replace a state's drift and band for
//! matching race ids; `*` in a pattern matches any run of characters and the pattern with the most
//! literal characters wins.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Hours between the lock and a state's poll close may differ from `lock_h` by at most this much.
const LOCK_H_TOLERANCE: f64 = 0.02;
/// No registered drift or band may exceed this many points (a typo guard).
const MAX_PTS: f64 = 50.0;

/// Failure to load the playbook.
#[derive(Debug, thiserror::Error)]
pub enum CountOrderError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing count-order table: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid count-order table: {0}")]
    Invalid(String),
}

/// Which party the count still to come favours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Lean {
    Dem,
    Rep,
}

impl Lean {
    /// `"DEM"` / `"REP"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Lean::Dem => "DEM",
            Lean::Rep => "REP",
        }
    }
}

/// Values at the five registered points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HourPoints {
    pub h1: f64,
    pub h3: f64,
    pub h6: f64,
    pub h12: f64,
    pub lock: f64,
}

impl HourPoints {
    fn values(&self) -> [f64; 5] {
        [self.h1, self.h3, self.h6, self.h12, self.lock]
    }
}

/// `[meta]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// The lock (Nov 4 2026 12:00 ET).
    pub lock_utc: DateTime<Utc>,
    /// When the table was registered (before any replay).
    pub registered: String,
    pub election_date: String,
}

/// A race-level override: replaces the state's drift and band.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RaceOverride {
    pub drift_pts: HourPoints,
    pub band_pts: HourPoints,
    pub notes: String,
}

/// One state's registered count order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateCountOrder {
    pub name: String,
    /// Hour zero of the state's points (its last general poll close, as the research counts it).
    pub poll_close_utc: DateTime<Utc>,
    /// Hours from poll close to the lock.
    pub lock_h: f64,
    /// Hours after poll close by which the count is essentially final (drift and band are zero after).
    pub final_h: f64,
    pub share_counted_at_lock: String,
    /// Vote types in the order they report.
    pub report_first: Vec<String>,
    pub drift_pts: HourPoints,
    pub band_pts: HourPoints,
    pub notes: String,
    /// Research memo section the numbers come from.
    pub source: String,
    /// Race id pattern → override.
    #[serde(default)]
    pub races: BTreeMap<String, RaceOverride>,
}

/// The whole table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountOrder {
    pub meta: Meta,
    /// USPS code → state.
    pub states: BTreeMap<String, StateCountOrder>,
}

/// What the table registers for a race at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Registered {
    /// Hours after the state's poll close.
    pub hours_after_close: f64,
    /// Expected partial-count drift still to unwind (points, D minus R; negative = count to come favours D).
    pub drift_pts: f64,
    /// Uncertainty band (points, non-negative).
    pub band_pts: f64,
}

impl Registered {
    /// The party the count still to come favours, when the drift exceeds its own band.
    pub fn favours(&self) -> Option<Lean> {
        if self.drift_pts.abs() <= self.band_pts {
            None
        } else if self.drift_pts < 0.0 {
            Some(Lean::Dem)
        } else {
            Some(Lean::Rep)
        }
    }
}

/// The registered curve that applies to one race (the state's, or a matching override).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile<'a> {
    /// USPS state code.
    pub state: &'a str,
    /// The state code, or the override pattern that matched.
    pub key: &'a str,
    pub poll_close_utc: DateTime<Utc>,
    pub lock_h: f64,
    pub final_h: f64,
    pub drift: &'a HourPoints,
    pub band: &'a HourPoints,
}

impl Profile<'_> {
    fn knots(&self) -> [f64; 6] {
        [1.0, 3.0, 6.0, 12.0, self.lock_h, self.final_h]
    }

    /// Registered drift and band at `now`; `None` before poll close.
    pub fn at(&self, now: DateTime<Utc>) -> Option<Registered> {
        self.at_ns(now.timestamp_nanos_opt()?)
    }

    /// [`Profile::at`] on a UTC nanosecond clock.
    pub fn at_ns(&self, now_ns: i64) -> Option<Registered> {
        let close = self.poll_close_utc.timestamp_nanos_opt()?;
        let h = now_ns.saturating_sub(close) as f64 / 3.6e12;
        (h.is_finite() && h >= 0.0).then(|| self.at_hour(h))
    }

    /// Registered drift and band `h` hours after poll close (`h >= 0`).
    pub fn at_hour(&self, h: f64) -> Registered {
        let knots = self.knots();
        let drift = interpolate(&knots, &with_final(self.drift), h);
        let band = interpolate(&knots, &with_final(self.band), h);
        Registered {
            hours_after_close: h,
            drift_pts: drift,
            band_pts: band.max(0.0),
        }
    }

    /// The registered lock-time drift and band.
    pub fn lock(&self) -> Registered {
        Registered {
            hours_after_close: self.lock_h,
            drift_pts: self.drift.lock,
            band_pts: self.band.lock,
        }
    }
}

fn with_final(p: &HourPoints) -> [f64; 6] {
    let v = p.values();
    [v[0], v[1], v[2], v[3], v[4], 0.0]
}

/// Piecewise-linear through `(knots[i], values[i])`, flat before the first knot, zero after the
/// last (the last value is zero by construction). Equal consecutive knots take the later value.
fn interpolate(knots: &[f64; 6], values: &[f64; 6], h: f64) -> f64 {
    if h <= knots[0] {
        return values[0];
    }
    for i in 1..knots.len() {
        if h <= knots[i] {
            let (x0, x1) = (knots[i - 1], knots[i]);
            if x1 - x0 <= f64::EPSILON {
                return values[i];
            }
            let t = (h - x0) / (x1 - x0);
            return values[i - 1] + t * (values[i] - values[i - 1]);
        }
    }
    0.0
}

/// The USPS state code of a race id (`"2026-HOUSE-CA-13"` → `"CA"`); `None` for non-state races
/// such as `"2026-CONTROL-SENATE"`.
pub fn state_of_race(race_id: &str) -> Option<&str> {
    let code = race_id.split('-').nth(2)?;
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())).then_some(code)
}

/// `*` matches any run of characters (including none); everything else is literal.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

impl CountOrder {
    /// Parse and validate TOML text.
    pub fn from_toml_str(text: &str) -> Result<Self, CountOrderError> {
        let table: CountOrder = toml::from_str(text)?;
        table.validate()?;
        Ok(table)
    }

    /// Load and validate a file.
    pub fn load(path: &Path) -> Result<Self, CountOrderError> {
        let text = std::fs::read_to_string(path).map_err(|source| CountOrderError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// The registered profile for a race: the longest matching override of its state, else the
    /// state's own row; `None` when the race's state is not in the table.
    pub fn profile(&self, race_id: &str) -> Option<Profile<'_>> {
        let (code, state) = self.states.get_key_value(state_of_race(race_id)?)?;
        let best = state
            .races
            .iter()
            .filter(|(pattern, _)| glob_match(pattern, race_id))
            .max_by_key(|(pattern, _)| pattern.chars().filter(|c| *c != '*').count());
        let (key, drift, band) = match best {
            Some((pattern, o)) => (pattern.as_str(), &o.drift_pts, &o.band_pts),
            None => (code.as_str(), &state.drift_pts, &state.band_pts),
        };
        Some(Profile {
            state: code,
            key,
            poll_close_utc: state.poll_close_utc,
            lock_h: state.lock_h,
            final_h: state.final_h,
            drift,
            band,
        })
    }

    /// Reject tables the executor could misread.
    pub fn validate(&self) -> Result<(), CountOrderError> {
        let bad = |m: String| Err(CountOrderError::Invalid(m));
        if self.states.is_empty() {
            return bad("no states".into());
        }
        for (code, s) in &self.states {
            if !(code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())) {
                return bad(format!("state key {code:?} is not a USPS code"));
            }
            if s.poll_close_utc > self.meta.lock_utc {
                return bad(format!("{code}: poll close after the lock"));
            }
            let to_lock = (self.meta.lock_utc - s.poll_close_utc).num_seconds() as f64 / 3600.0;
            if !(s.lock_h.is_finite() && (s.lock_h - to_lock).abs() <= LOCK_H_TOLERANCE) {
                return bad(format!(
                    "{code}: lock_h {} but the lock is {to_lock:.2} h after poll close",
                    s.lock_h
                ));
            }
            if s.lock_h < 12.0 {
                return bad(format!("{code}: lock_h {} is before the +12 h point", s.lock_h));
            }
            if !(s.final_h.is_finite() && s.final_h > s.lock_h) {
                return bad(format!("{code}: final_h {} must be after lock_h", s.final_h));
            }
            if s.report_first.is_empty() || s.report_first.iter().any(|v| v.trim().is_empty()) {
                return bad(format!("{code}: report_first must list the vote types in order"));
            }
            if s.source.trim().is_empty() || s.name.trim().is_empty() {
                return bad(format!("{code}: name and source are required"));
            }
            check_points(code, s.lock_h, &s.drift_pts, &s.band_pts)?;
            for (pattern, o) in &s.races {
                if state_of_race(&pattern.replace('*', "X")) != Some(code.as_str()) {
                    return bad(format!(
                        "{code}: override {pattern:?} must be a race id pattern of the same state"
                    ));
                }
                check_points(&format!("{code} {pattern}"), s.lock_h, &o.drift_pts, &o.band_pts)?;
            }
        }
        Ok(())
    }
}

fn check_points(
    what: &str,
    lock_h: f64,
    drift: &HourPoints,
    band: &HourPoints,
) -> Result<(), CountOrderError> {
    for v in drift.values() {
        if !(v.is_finite() && v.abs() <= MAX_PTS) {
            return Err(CountOrderError::Invalid(format!(
                "{what}: drift {v} out of range"
            )));
        }
    }
    for v in band.values() {
        if !(v.is_finite() && (0.0..=MAX_PTS).contains(&v)) {
            return Err(CountOrderError::Invalid(format!(
                "{what}: band {v} must be in [0, {MAX_PTS}]"
            )));
        }
    }
    // Where the lock coincides with the +12 h point (Alaska) the two must agree.
    if (lock_h - 12.0).abs() <= f64::EPSILON && (drift.h12 != drift.lock || band.h12 != band.lock) {
        return Err(CountOrderError::Invalid(format!(
            "{what}: lock is +12 h, so h12 and lock must be equal"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_file(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
    }

    fn shipped() -> CountOrder {
        CountOrder::load(&repo_file("config/count_order.toml")).expect("shipped count-order table")
    }

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("timestamp")
    }

    #[test]
    fn the_shipped_table_parses_with_all_18_states() {
        let t = shipped();
        let codes: Vec<&str> = t.states.keys().map(String::as_str).collect();
        assert_eq!(
            codes,
            vec![
                "AK", "AZ", "CA", "GA", "IA", "KS", "ME", "MI", "NC", "NE", "NH", "NV", "OH", "PA", "SC",
                "TX", "WA", "WI"
            ]
        );
        assert_eq!(t.meta.lock_utc, ts("2026-11-04T17:00:00Z"));
        for (code, s) in &t.states {
            assert!(!s.notes.is_empty(), "{code} notes");
            assert!(s.source.contains("2026-10-06"), "{code} cites its memo");
        }
    }

    /// The headline numbers of the research memos survive into the table.
    #[test]
    fn the_shipped_table_carries_the_research_headlines() {
        let t = shipped();
        let lock = |race: &str| t.profile(race).expect(race).lock();
        // CA about -3 statewide, House seats -2 to -4; NV -1.5 to -2.5; PA -1 to -2.
        assert_eq!(lock("2026-GOV-CA").drift_pts, -3.0);
        assert_eq!(lock("2026-HOUSE-CA-22").drift_pts, -3.0);
        assert_eq!(lock("2026-HOUSE-CA-22").band_pts, 1.5);
        assert_eq!(lock("2026-GOV-NV").drift_pts, -2.0);
        assert_eq!(lock("2026-HOUSE-PA-07").drift_pts, -1.5);
        for race in ["2026-GOV-CA", "2026-HOUSE-CA-48", "2026-GOV-NV", "2026-GOV-PA"] {
            assert_eq!(
                lock(race).favours(),
                Some(Lean::Dem),
                "{race}: the count to come favours D"
            );
        }
        // ME the other way.
        assert_eq!(lock("2026-HOUSE-ME-02").favours(), Some(Lean::Rep));
        // AZ small and uncertain, WA-03 ±5 with no direction, fast-count states near zero.
        assert_eq!(lock("2026-SEN-AK").favours(), None);
        assert_eq!(lock("2026-GOV-AZ").favours(), None);
        let wa3 = lock("2026-HOUSE-WA-03");
        assert_eq!((wa3.drift_pts, wa3.band_pts, wa3.favours()), (0.0, 5.0, None));
        for race in [
            "2026-SEN-NC",
            "2026-SEN-OH",
            "2026-SEN-TX",
            "2026-SEN-NH",
            "2026-SEN-IA",
            "2026-SEN-SC",
        ] {
            let l = lock(race);
            assert!(l.drift_pts.abs() <= 0.3, "{race} lock drift {}", l.drift_pts);
            assert_eq!(l.favours(), None, "{race}");
        }
        // MI and WI overnight traps: Democratic remaining count at +3 h, gone by the lock.
        let mi = t.profile("2026-SEN-MI").expect("MI");
        assert_eq!(mi.at_hour(3.0).favours(), Some(Lean::Dem));
        assert_eq!(mi.lock().favours(), None);
        let wi = t.profile("2026-GOV-WI").expect("WI");
        assert_eq!(wi.at_hour(3.0).favours(), Some(Lean::Dem));
        assert_eq!(wi.lock().favours(), None);
        // NC first-hour Democratic mirage: the count to come favours R.
        let nc = t.profile("2026-SEN-NC").expect("NC");
        assert_eq!(nc.at_hour(0.5).favours(), Some(Lean::Rep));
        assert!(nc.at_hour(0.5).drift_pts >= 15.0);
    }

    /// Every state's poll close is one of its closings in `config/poll_closing.toml`.
    #[test]
    fn poll_closes_agree_with_the_poll_closing_table() {
        let t = shipped();
        let text = std::fs::read_to_string(repo_file("config/poll_closing.toml")).expect("poll_closing.toml");
        let closing: toml::Value = toml::from_str(&text).expect("toml");
        for (code, s) in &t.states {
            let closes: Vec<DateTime<Utc>> = closing["states"][code.as_str()]["closings"]
                .as_array()
                .expect("closings")
                .iter()
                .map(|c| ts(c["closes_utc"].as_str().expect("closes_utc")))
                .collect();
            assert!(
                closes.contains(&s.poll_close_utc),
                "{code}: {} is not in {closes:?}",
                s.poll_close_utc
            );
        }
    }

    #[test]
    fn interpolates_between_points_and_goes_to_zero_after_final() {
        let t = shipped();
        let nv = t.profile("2026-GOV-NV").expect("NV");
        let close = ts("2026-11-04T03:00:00Z");
        assert_eq!(
            nv.at(close - chrono::Duration::seconds(1)),
            None,
            "nothing before close"
        );
        let at = |h: f64| nv.at_hour(h);
        assert_eq!(at(0.0).drift_pts, 2.5, "first value held before +1 h");
        assert_eq!(at(1.0).drift_pts, 2.5);
        assert!((at(2.0).drift_pts - (2.5 + 0.5 * (-2.25 - 2.5))).abs() < 1e-12);
        assert!((at(9.0).drift_pts - (-2.65 + 0.5 * (-2.4 + 2.65))).abs() < 1e-12);
        assert_eq!(at(14.0).drift_pts, -2.0);
        assert_eq!(at(14.0).band_pts, 0.75);
        // Between the lock (+14) and final (+168): linear to zero.
        let mid = at(91.0);
        assert!((mid.drift_pts - (-1.0)).abs() < 1e-12);
        assert!((mid.band_pts - 0.375).abs() < 1e-12);
        assert_eq!(at(168.0).drift_pts, 0.0);
        assert_eq!(at(500.0).drift_pts, 0.0);
        assert_eq!(at(500.0).band_pts, 0.0);
        // `at` agrees with `at_hour` on the clock.
        let r = nv.at(close + chrono::Duration::minutes(90)).expect("after close");
        assert!((r.hours_after_close - 1.5).abs() < 1e-12);
        assert_eq!(r.drift_pts, at(1.5).drift_pts);
        // Alaska: lock at +12 coincides with h12.
        let ak = t.profile("2026-SEN-AK").expect("AK");
        assert_eq!(ak.at_hour(12.0).band_pts, 1.0);
    }

    #[test]
    fn overrides_match_by_pattern_and_the_most_specific_wins() {
        let t = shipped();
        assert_eq!(t.profile("2026-HOUSE-CA-13").expect("CA").key, "2026-HOUSE-CA-*");
        assert_eq!(t.profile("2026-GOV-CA").expect("CA").key, "CA");
        assert_eq!(t.profile("2026-HOUSE-WA-03").expect("WA").key, "2026-HOUSE-WA-03");
        assert_eq!(t.profile("2026-HOUSE-PA-01").expect("PA").key, "PA");
        assert!(t.profile("2026-SEN-FL-S").is_none(), "FL is not registered");
        assert!(t.profile("2026-CONTROL-SENATE").is_none());
        assert_eq!(state_of_race("2026-SEN-OH-S"), Some("OH"));
        assert_eq!(state_of_race("2026-CONTROL-HOUSE"), None);
        assert!(glob_match("2026-HOUSE-CA-*", "2026-HOUSE-CA-"));
        assert!(glob_match("2026-*-CA-*3", "2026-HOUSE-CA-13"));
        assert!(!glob_match("2026-*-CA-*3", "2026-HOUSE-CA-12"));
        assert!(!glob_match("2026-HOUSE-CA-13", "2026-HOUSE-CA-1"));

        let mut more = t.clone();
        let o = more.states["CA"].races["2026-HOUSE-CA-*"].clone();
        more.states
            .get_mut("CA")
            .expect("CA")
            .races
            .insert("2026-HOUSE-CA-13".into(), o);
        assert_eq!(
            more.profile("2026-HOUSE-CA-13").expect("CA").key,
            "2026-HOUSE-CA-13"
        );
        assert_eq!(
            more.profile("2026-HOUSE-CA-22").expect("CA").key,
            "2026-HOUSE-CA-*"
        );
    }

    #[test]
    fn rejects_broken_tables() {
        let text = std::fs::read_to_string(repo_file("config/count_order.toml")).expect("file");
        for (from, to) in [
            // lock_h inconsistent with poll close and the lock.
            ("lock_h = 14.0", "lock_h = 15.0"),
            // A negative band.
            (
                "band_pts = { h1 = 2.0, h3 = 1.25, h6 = 0.9, h12 = 1.15, lock = 0.75 }",
                "band_pts = { h1 = 2.0, h3 = 1.25, h6 = 0.9, h12 = 1.15, lock = -0.75 }",
            ),
            // A drift far outside anything observed.
            (
                "drift_pts = { h1 = 2.5, h3 = -2.25, h6 = -2.65, h12 = -2.4, lock = -2.0 }",
                "drift_pts = { h1 = 2.5, h3 = -2.25, h6 = -2.65, h12 = -2.4, lock = -200.0 }",
            ),
            // An override keyed to another state's races.
            (
                "[states.WA.races.\"2026-HOUSE-WA-03\"]",
                "[states.WA.races.\"2026-HOUSE-OR-03\"]",
            ),
            // final before the lock.
            ("final_h = 168.0", "final_h = 10.0"),
            // A misspelt point is refused, not defaulted.
            ("lock = -2.0 }", "lok = -2.0 }"),
        ] {
            let broken = text.replacen(from, to, 1);
            assert_ne!(broken, text, "{from} not found");
            assert!(
                CountOrder::from_toml_str(&broken).is_err(),
                "{to} should be refused"
            );
        }
        assert!(matches!(
            CountOrder::load(Path::new("/nonexistent/count_order.toml")),
            Err(CountOrderError::Io { .. })
        ));
    }
}
