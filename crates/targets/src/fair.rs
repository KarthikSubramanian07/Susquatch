//! The live fair feed: `state/fair_live.json`, rewritten atomically about every 15 s by the
//! research scheduler (`py/susq/fair/service.py`) as a `susq.v1.TargetFile` holding only `header`
//! and `fair`.
//!
//! Staleness is judged from `header.produced_at_ns` and nothing else (the file's mtime is only used
//! to avoid re-parsing an unchanged file). A malformed or wrong-schema file is rejected and the last
//! good values are kept, but they keep ageing: consumers see them go stale after
//! `[fair].stale_after_s` and cancel what they built on them.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use susq_contracts::v1::{FairValue, HealthItem, TargetFile};
use susq_core::clock::duration_ns;

use crate::{error::ContentError, validate::validate_content};

/// Default location relative to `$SUSQ_HOME`.
pub const DEFAULT_PATH: &str = "state/fair_live.json";

/// Why a fair file was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FairReject {
    #[error("{0} missing")]
    Missing(PathBuf),
    #[error("unreadable: {0}")]
    Unreadable(String),
    #[error("unparseable: {0}")]
    Unparseable(String),
    #[error("missing header")]
    MissingHeader,
    #[error("schema version {got} (want {want})")]
    SchemaVersion { got: u32, want: u32 },
    #[error("produced {0} s in the future")]
    FromTheFuture(i64),
    #[error("carries target positions: not a fair-only file")]
    NotFairOnly,
    #[error("invalid fair value: {0}")]
    Invalid(ContentError),
}

/// A validated fair file.
#[derive(Debug, Clone, PartialEq)]
pub struct FairSnapshot {
    pub produced_at_ns: i64,
    pub seq: u64,
    pub fair: Vec<FairValue>,
}

/// What one [`FairReader::refresh`] saw.
#[derive(Debug, Clone, PartialEq)]
pub enum FairOutcome {
    /// Same file as last time.
    Unchanged,
    /// A new valid file (number of fair values).
    Loaded(usize),
    /// The file on disk was refused; the last good one is kept (and keeps ageing).
    Rejected(FairReject),
}

/// Polls and validates the fair file.
#[derive(Debug)]
pub struct FairReader {
    path: PathBuf,
    stale_after: Duration,
    future_skew: Duration,
    seen: Option<(SystemTime, u64)>,
    current: Option<FairSnapshot>,
    rejection: Option<FairReject>,
}

impl FairReader {
    /// Read `path`; values older than `stale_after` are stale.
    pub fn new(path: impl Into<PathBuf>, stale_after: Duration) -> Self {
        Self {
            path: path.into(),
            stale_after,
            future_skew: Duration::from_secs(5 * 60),
            seen: None,
            current: None,
            rejection: None,
        }
    }

    /// Re-read the file if it changed on disk.
    pub fn refresh(&mut self, now_ns: i64) -> FairOutcome {
        let stamp = match std::fs::metadata(&self.path) {
            Ok(m) => (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return self.reject(FairReject::Missing(self.path.clone()), None);
            }
            Err(e) => return self.reject(FairReject::Unreadable(e.to_string()), None),
        };
        if self.seen == Some(stamp) {
            return FairOutcome::Unchanged;
        }
        match read(&self.path, now_ns, self.future_skew) {
            Ok(snap) => {
                let n = snap.fair.len();
                self.seen = Some(stamp);
                self.current = Some(snap);
                self.rejection = None;
                FairOutcome::Loaded(n)
            }
            Err(r) => self.reject(r, Some(stamp)),
        }
    }

    fn reject(&mut self, reason: FairReject, stamp: Option<(SystemTime, u64)>) -> FairOutcome {
        self.seen = stamp;
        if self.rejection.as_ref() == Some(&reason) {
            return FairOutcome::Unchanged; // same problem as last time: don't flood the log
        }
        tracing::warn!(path = %self.path.display(), %reason, "fair file rejected; keeping last good values");
        self.rejection = Some(reason.clone());
        FairOutcome::Rejected(reason)
    }

    /// The last good file, however old (consumers judge freshness per value).
    pub fn current(&self) -> Option<&FairSnapshot> {
        self.current.as_ref()
    }

    /// The last good file if it is still fresh at `now_ns`.
    pub fn fresh(&self, now_ns: i64) -> Option<&FairSnapshot> {
        self.current
            .as_ref()
            .filter(|s| now_ns - s.produced_at_ns <= duration_ns(self.stale_after))
    }

    /// Dashboard entry (`fair`).
    pub fn health(&self, now_ns: i64) -> HealthItem {
        let age_s = |s: &FairSnapshot| (now_ns - s.produced_at_ns) / 1_000_000_000;
        let (state, detail) = match (&self.current, self.fresh(now_ns), &self.rejection) {
            (_, Some(s), None) => ("ok", format!("{} values, {} s old", s.fair.len(), age_s(s))),
            (_, Some(s), Some(r)) => (
                "degraded",
                format!("rejected: {r}; using values from {} s ago", age_s(s)),
            ),
            (Some(s), None, r) => (
                "down",
                format!(
                    "stale: {} s old (limit {} s){}",
                    age_s(s),
                    self.stale_after.as_secs(),
                    r.as_ref().map(|r| format!("; rejected: {r}")).unwrap_or_default()
                ),
            ),
            (None, None, r) => (
                "down",
                r.as_ref().map_or("no fair file yet".into(), |r| r.to_string()),
            ),
        };
        let last_ok_ns = self.fresh(now_ns).map_or(0, |_| now_ns);
        HealthItem {
            name: "fair".into(),
            state: state.into(),
            detail,
            last_ok_ns,
        }
    }

    /// Where the file is read from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read(path: &Path, now_ns: i64, future_skew: Duration) -> Result<FairSnapshot, FairReject> {
    let text = std::fs::read_to_string(path).map_err(|e| FairReject::Unreadable(e.to_string()))?;
    let file: TargetFile =
        susq_contracts::from_json(&text).map_err(|e| FairReject::Unparseable(e.to_string()))?;
    let header = file.header.as_ref().ok_or(FairReject::MissingHeader)?;
    if header.schema_version != susq_contracts::SCHEMA_VERSION {
        return Err(FairReject::SchemaVersion {
            got: header.schema_version,
            want: susq_contracts::SCHEMA_VERSION,
        });
    }
    if header.produced_at_ns - now_ns > duration_ns(future_skew) {
        return Err(FairReject::FromTheFuture(
            (header.produced_at_ns - now_ns) / 1_000_000_000,
        ));
    }
    if !file.positions.is_empty() {
        return Err(FairReject::NotFairOnly);
    }
    validate_content(&file).map_err(FairReject::Invalid)?;
    Ok(FairSnapshot {
        produced_at_ns: header.produced_at_ns,
        seq: header.seq,
        fair: file.fair,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_contracts::v1::{Header, Producer, TargetPosition};

    const S: i64 = 1_000_000_000;
    const T0: i64 = 1_791_475_200 * S;

    /// Expected-rejection predicate for a malformed-file case.
    type Check = fn(&FairReject) -> bool;

    fn fv(ex: &str, p: f64) -> FairValue {
        FairValue {
            exchange_id: ex.into(),
            p_yes: p,
            p_lo: p,
            p_hi: p,
            source: "0.8*kalshi+0.2*sim".into(),
            ..Default::default()
        }
    }

    fn file(produced_at_ns: i64, fair: Vec<FairValue>) -> TargetFile {
        TargetFile {
            header: Some(Header {
                schema_version: susq_contracts::SCHEMA_VERSION,
                produced_at_ns,
                producer: Some(Producer {
                    name: "python-scheduler".into(),
                    git_sha: "t".into(),
                }),
                seq: 7,
            }),
            fair,
            ..Default::default()
        }
    }

    fn write(path: &Path, text: &str) {
        // Mirror the producer's atomic tmp + rename so the reader sees a new inode/mtime.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).expect("write");
        std::fs::rename(tmp, path).expect("rename");
    }

    fn write_file(path: &Path, tf: &TargetFile) {
        write(path, &susq_contracts::to_json(tf).expect("json"));
    }

    fn reader(dir: &Path) -> FairReader {
        FairReader::new(dir.join("fair_live.json"), Duration::from_secs(60))
    }

    #[test]
    fn loads_fresh_values_and_notices_staleness() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut r = reader(dir.path());
        assert!(matches!(
            r.refresh(T0),
            FairOutcome::Rejected(FairReject::Missing(_))
        ));
        assert_eq!(r.health(T0).state, "down");

        write_file(
            &dir.path().join("fair_live.json"),
            &file(T0, vec![fv("1", 0.62), fv("2", 0.3)]),
        );
        assert_eq!(r.refresh(T0 + S), FairOutcome::Loaded(2));
        assert_eq!(r.refresh(T0 + 2 * S), FairOutcome::Unchanged);
        assert!(r.fresh(T0 + 60 * S).is_some());
        assert_eq!(r.health(T0 + 30 * S).state, "ok");
        // No new file for over a minute: stale, but still readable as history.
        assert!(r.fresh(T0 + 61 * S).is_none());
        assert!(r.current().is_some());
        assert_eq!(r.health(T0 + 61 * S).state, "down");
    }

    #[test]
    fn malformed_files_are_rejected_and_the_last_good_one_keeps_ageing() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("fair_live.json");
        let mut r = reader(dir.path());
        write_file(&path, &file(T0, vec![fv("1", 0.62)]));
        assert_eq!(r.refresh(T0), FairOutcome::Loaded(1));

        write(&path, "{\"header\": {\"schemaVersion\": 1, \"produ");
        assert!(matches!(
            r.refresh(T0 + S),
            FairOutcome::Rejected(FairReject::Unparseable(_))
        ));
        assert_eq!(r.current().map(|s| s.fair[0].p_yes), Some(0.62), "last good kept");
        assert_eq!(r.health(T0 + S).state, "degraded");
        assert!(r.fresh(T0 + 61 * S).is_none(), "and it still goes stale");

        let cases: Vec<(TargetFile, Check)> = vec![
            // NaN / infinity are refused either while parsing or by validation.
            (file(T0, vec![fv("1", f64::NAN)]), |r| {
                matches!(r, FairReject::Invalid(_) | FairReject::Unparseable(_))
            }),
            (file(T0, vec![fv("1", f64::INFINITY)]), |r| {
                matches!(r, FairReject::Invalid(_) | FairReject::Unparseable(_))
            }),
            (file(T0, vec![fv("1", 1.2)]), |r| {
                matches!(r, FairReject::Invalid(_))
            }),
            (file(T0, vec![fv("1", 0.4), fv("1", 0.5)]), |r| {
                matches!(r, FairReject::Invalid(_))
            }),
            (
                TargetFile {
                    header: None,
                    ..file(T0, vec![fv("1", 0.5)])
                },
                |r| *r == FairReject::MissingHeader,
            ),
            (file(T0 + 3600 * S, vec![fv("1", 0.5)]), |r| {
                matches!(r, FairReject::FromTheFuture(_))
            }),
            (
                TargetFile {
                    positions: vec![TargetPosition::default()],
                    ..file(T0, vec![fv("1", 0.5)])
                },
                |r| *r == FairReject::NotFairOnly,
            ),
        ];
        for (i, (tf, check)) in cases.into_iter().enumerate() {
            // A fresh reader per case, primed with a good file, so each rejection is reported.
            let case_dir = tempfile::tempdir().expect("tmp");
            let case_path = case_dir.path().join("fair_live.json");
            let mut cr = reader(case_dir.path());
            write_file(&case_path, &file(T0, vec![fv("1", 0.62)]));
            assert_eq!(cr.refresh(T0), FairOutcome::Loaded(1));
            write(
                &case_path,
                &format!("{} ", susq_contracts::to_json(&tf).expect("json")),
            );
            match cr.refresh(T0 + 2 * S) {
                FairOutcome::Rejected(reason) => assert!(check(&reason), "case {i}: {reason:?}"),
                other => panic!("case {i}: expected a rejection, got {other:?}"),
            }
            assert_eq!(
                cr.current().map(|s| s.fair[0].p_yes),
                Some(0.62),
                "case {i}: last good kept"
            );
        }
        let mut wrong = file(T0, vec![fv("1", 0.5)]);
        if let Some(h) = wrong.header.as_mut() {
            h.schema_version = 99;
        }
        write_file(&path, &wrong);
        assert!(matches!(
            r.refresh(T0 + 3 * S),
            FairOutcome::Rejected(FairReject::SchemaVersion { got: 99, .. })
        ));
        assert_eq!(
            r.current().map(|s| s.fair.len()),
            Some(1),
            "still the original good file"
        );
    }

    #[test]
    fn repeated_identical_problems_are_reported_once() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut r = reader(dir.path());
        assert!(matches!(r.refresh(T0), FairOutcome::Rejected(_)));
        assert_eq!(r.refresh(T0 + S), FairOutcome::Unchanged);
    }

    #[test]
    fn reads_what_the_python_producer_writes() {
        // Shape written by susq.fair.service.fair_file (proto3 JSON, header + fair only).
        let dir = tempfile::tempdir().expect("tmp");
        let text = format!(
            r#"{{
  "header": {{"schemaVersion": 1, "producedAtNs": "{T0}", "producer": {{"name": "python-scheduler", "gitSha": "abc"}}, "seq": "12"}},
  "fair": [{{"exchangeId": "381", "marketId": "9", "pYes": 0.455, "pLo": 0.41, "pHi": 0.5, "source": "0.8*kalshi+0.2*sim"}}]
}}"#
        );
        write(&dir.path().join("fair_live.json"), &text);
        let mut r = reader(dir.path());
        assert_eq!(r.refresh(T0), FairOutcome::Loaded(1));
        let s = r.fresh(T0).expect("fresh");
        assert_eq!(
            (s.seq, s.fair[0].exchange_id.as_str(), s.fair[0].p_yes),
            (12, "381", 0.455)
        );
    }
}
