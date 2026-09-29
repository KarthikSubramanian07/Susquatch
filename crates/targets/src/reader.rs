//! The stateful reader: caches the last verified file, re-checks freshness on every refresh.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use susq_contracts::v1::HealthItem;
use susq_core::{Config, clock::duration_ns};

use crate::{
    error::{RejectReason, span},
    source::{self, POINTER_FILE, Resolved},
    targets::Targets,
    validate::{Limits, check_fresh},
};

/// Where to read targets from and what counts as acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderConfig {
    /// Directory holding `current.json` and the `targets-*.json` files.
    pub dir: PathBuf,
    /// Oldest acceptable `now - header.produced_at_ns`.
    pub max_age: Duration,
    /// The only `tournament_slug` accepted.
    pub tournament_slug: String,
    /// How far `header.produced_at_ns` may lie in the future (clock skew between hosts).
    pub future_skew: Duration,
}

impl ReaderConfig {
    /// Default tolerated clock skew for `produced_at_ns` in the future.
    pub const DEFAULT_FUTURE_SKEW: Duration = Duration::from_secs(5 * 60);

    /// Config with [`Self::DEFAULT_FUTURE_SKEW`].
    pub fn new(dir: impl Into<PathBuf>, max_age: Duration, tournament_slug: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            max_age,
            tournament_slug: tournament_slug.into(),
            future_skew: Self::DEFAULT_FUTURE_SKEW,
        }
    }

    /// From `config/susq.toml`: `[targets] dir` (resolved against `root`), `max_age_h`, and
    /// `[platform] tournament_slug`.
    pub fn from_core(cfg: &Config, root: &Path) -> Self {
        Self::new(
            root.join(&cfg.targets.dir),
            cfg.targets_max_age(),
            cfg.platform.tournament_slug.clone(),
        )
    }

    fn limits(&self) -> Limits {
        Limits {
            max_age_ns: duration_ns(self.max_age),
            future_skew_ns: duration_ns(self.future_skew),
        }
    }
}

/// What one [`TargetsReader::refresh`] observed.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshOutcome {
    /// Nothing new on disk (or the same problem as last time). Freshness was still re-evaluated.
    Unchanged,
    /// A fresh, valid file was adopted; increasing exposure is allowed again.
    Loaded(Arc<Targets>),
    /// The latest `current.json` was refused; the last good targets are kept, increases are blocked.
    Rejected(RejectReason),
}

/// The bytes last seen in `current.json` and the file they resolved to.
#[derive(Debug)]
struct Seen {
    pointer: Vec<u8>,
    resolved: Option<Resolved>,
}

/// Polls `targets/current.json`, keeps the last good [`Targets`], and decides whether exposure may grow.
///
/// See the crate docs for the fail-safe rule.
#[derive(Debug)]
pub struct TargetsReader {
    cfg: ReaderConfig,
    pointer_path: PathBuf,
    limits: Limits,
    seen: Option<Seen>,
    /// Statically valid targets for `seen` that failed only the time checks (may still become valid).
    pending: Option<Arc<Targets>>,
    good: Option<Arc<Targets>>,
    rejection: Option<RejectReason>,
    stale: Option<RejectReason>,
    last_ok_ns: i64,
}

impl TargetsReader {
    /// A reader that has not looked at the disk yet (no targets, increases blocked).
    pub fn new(cfg: ReaderConfig) -> Self {
        Self {
            pointer_path: cfg.dir.join(POINTER_FILE),
            limits: cfg.limits(),
            cfg,
            seen: None,
            pending: None,
            good: None,
            rejection: None,
            stale: None,
            last_ok_ns: 0,
        }
    }

    /// The configuration this reader was built with.
    pub fn config(&self) -> &ReaderConfig {
        &self.cfg
    }

    /// Look at `current.json` and re-evaluate freshness at `now_ns`.
    ///
    /// The target file is only re-read and rehashed when the pointer bytes or the named file's
    /// size/mtime changed. A given problem is reported as `Rejected` once; while it persists,
    /// later refreshes return `Unchanged`.
    pub fn refresh(&mut self, now_ns: i64) -> RefreshOutcome {
        let outcome = match source::read_pointer(&self.pointer_path) {
            Err(reason) => self.pointer_failed(reason),
            Ok(bytes) if self.is_unchanged(&bytes) => self.recheck(now_ns),
            Ok(bytes) => self.evaluate(bytes, now_ns),
        };
        self.stale = self
            .good
            .as_ref()
            .and_then(|g| check_fresh(g.file(), now_ns, self.limits).err());
        if self.increase_exposure_allowed() {
            self.last_ok_ns = now_ns;
        }
        outcome
    }

    /// Last good targets, even if stale or superseded by a rejected file. `None` until one loads.
    pub fn current(&self) -> Option<&Arc<Targets>> {
        self.good.as_ref()
    }

    /// True only when the last good targets came from the latest `current.json` and were fresh at the
    /// last [`Self::refresh`]. Reducing exposure never needs this.
    pub fn increase_exposure_allowed(&self) -> bool {
        self.good.is_some() && self.rejection.is_none() && self.stale.is_none()
    }

    /// Why the latest `current.json` is not in use, if it is not.
    pub fn rejection(&self) -> Option<&RejectReason> {
        self.rejection.as_ref()
    }

    /// Why the last good targets were no longer fresh at the last refresh, if they were not.
    pub fn stale_reason(&self) -> Option<&RejectReason> {
        self.stale.as_ref()
    }

    /// Dashboard item `targets`: `ok`, `degraded` (keeping old targets, increases blocked) or `down`
    /// (nothing loaded). Freshness is evaluated at `now_ns`.
    pub fn health(&self, now_ns: i64) -> HealthItem {
        let item = |state: &str, detail: String, last_ok_ns: i64| HealthItem {
            name: "targets".into(),
            state: state.into(),
            detail,
            last_ok_ns,
        };
        let Some(good) = &self.good else {
            let why = self
                .rejection
                .as_ref()
                .map_or_else(|| "not read yet".to_owned(), ToString::to_string);
            return item("down", format!("no targets loaded: {why}"), self.last_ok_ns);
        };
        let age = span(now_ns.saturating_sub(good.produced_at_ns()));
        let problem = match (&self.rejection, check_fresh(good.file(), now_ns, self.limits)) {
            (Some(r), _) => Some(format!("rejected: {r}")),
            (None, Err(s)) => Some(format!("stale: {s}")),
            (None, Ok(())) => None,
        };
        match problem {
            Some(p) => item(
                "degraded",
                format!("{p}; keeping {} from {age} ago", good.source()),
                self.last_ok_ns,
            ),
            None => {
                let left = span(good.valid_until_ns().saturating_sub(now_ns));
                item(
                    "ok",
                    format!("{} from {age} ago, valid for {left}", good.source()),
                    now_ns,
                )
            }
        }
    }

    fn is_unchanged(&self, pointer: &[u8]) -> bool {
        self.seen
            .as_ref()
            .is_some_and(|s| s.pointer == pointer && s.resolved.as_ref().is_none_or(Resolved::is_current))
    }

    fn pointer_failed(&mut self, reason: RejectReason) -> RefreshOutcome {
        self.seen = None;
        self.pending = None;
        if self.rejection.as_ref() == Some(&reason) {
            return RefreshOutcome::Unchanged;
        }
        self.reject(reason)
    }

    /// Same bytes on disk: only a pending (time-rejected) candidate can change the picture.
    fn recheck(&mut self, now_ns: i64) -> RefreshOutcome {
        let Some(pending) = self.pending.clone() else {
            return RefreshOutcome::Unchanged;
        };
        match check_fresh(pending.file(), now_ns, self.limits) {
            Ok(()) => self.accept(pending),
            Err(reason) => {
                self.rejection = Some(reason);
                RefreshOutcome::Unchanged
            }
        }
    }

    fn evaluate(&mut self, pointer: Vec<u8>, now_ns: i64) -> RefreshOutcome {
        let eval = source::evaluate(&self.cfg.dir, &pointer, &self.cfg.tournament_slug);
        self.seen = Some(Seen {
            pointer,
            resolved: eval.resolved,
        });
        self.pending = None;
        let candidate = match eval.result {
            Ok(targets) => Arc::new(targets),
            Err(reason) => return self.reject(reason),
        };
        match check_fresh(candidate.file(), now_ns, self.limits) {
            Ok(()) => self.accept(candidate),
            Err(reason) => {
                self.pending = Some(candidate);
                self.reject(reason)
            }
        }
    }

    fn accept(&mut self, targets: Arc<Targets>) -> RefreshOutcome {
        tracing::info!(
            file = targets.source(),
            produced_at_ns = targets.produced_at_ns(),
            "targets loaded"
        );
        self.pending = None;
        self.rejection = None;
        self.good = Some(Arc::clone(&targets));
        RefreshOutcome::Loaded(targets)
    }

    fn reject(&mut self, reason: RejectReason) -> RefreshOutcome {
        tracing::warn!(
            %reason,
            keeping = self.good.as_ref().map(|g| g.source()),
            "targets rejected; increases blocked"
        );
        self.rejection = Some(reason.clone());
        RefreshOutcome::Rejected(reason)
    }
}
