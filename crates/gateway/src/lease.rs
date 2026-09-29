//! The single-trader lease: `ops/state/trader.lease`, a `susq.v1.TraderLease` JSON document.
//!
//! Only the holder of an unexpired lease may place or cancel orders. The protocol, shared with the
//! Python bridge (same file, same contract):
//!
//! 1. Take an exclusive `flock` on the sidecar `trader.lease.lock` (never on the lease file itself:
//!    the atomic rename below replaces its inode).
//! 2. Read the lease. It may be taken if it is missing, unparseable, expired
//!    (`expires_at_ns <= now`), or already ours (same holder, host and pid).
//! 3. Write the new lease to `trader.lease.tmp`, fsync, rename over `trader.lease`, fsync the
//!    directory. `epoch` increments whenever the holder changes.
//! 4. Release the lock. Renew every `renew_s` with `expires_at = now + ttl_s`.
//!
//! Releasing writes the same lease with `expires_at_ns = now`, so the other side can take it at once.
//! A holder considers itself leased only while `now < expires_at - safety margin`.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use fs2::FileExt;
use parking_lot::Mutex;
use susq_contracts::v1::TraderLease;
use susq_core::{Clock, clock::duration_ns};

/// Margin before expiry at which we stop trusting our own lease.
pub const SAFETY_MARGIN: Duration = Duration::from_secs(2);

/// Who we are and how long a lease lasts.
#[derive(Debug, Clone)]
pub struct LeaseConfig {
    pub path: PathBuf,
    pub ttl: Duration,
    /// `rust-live` (the bridge uses `bridge`).
    pub holder: String,
    pub host: String,
    pub pid: u32,
}

/// Result of an acquire/renew attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum LeaseStatus {
    /// We hold it until `expires_at_ns`.
    Held(TraderLease),
    /// Someone else holds an unexpired lease.
    HeldBy(TraderLease),
}

/// Manages our side of the lease. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Lease {
    cfg: LeaseConfig,
    clock: Arc<dyn Clock>,
    ours: Arc<Mutex<Option<TraderLease>>>,
}

impl Lease {
    /// A lease manager that holds nothing yet.
    pub fn new(cfg: LeaseConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            cfg,
            clock,
            ours: Arc::default(),
        }
    }

    fn lock_path(&self) -> PathBuf {
        sidecar(&self.cfg.path, "lock")
    }

    fn is_ours(&self, l: &TraderLease) -> bool {
        l.holder == self.cfg.holder && l.host == self.cfg.host && l.pid == self.cfg.pid
    }

    /// Acquire or renew. Never blocks on another holder: returns `HeldBy` instead.
    pub fn acquire(&self) -> io::Result<LeaseStatus> {
        let _lock = self.flock()?;
        let now = self.clock.now_ns();
        let current = read_lease(&self.cfg.path)?;
        if let Some(cur) = current
            .as_ref()
            .filter(|c| c.expires_at_ns > now && !self.is_ours(c))
        {
            *self.ours.lock() = None;
            return Ok(LeaseStatus::HeldBy(cur.clone()));
        }
        let (acquired_at_ns, epoch) = match &current {
            Some(c) if self.is_ours(c) => (c.acquired_at_ns, c.epoch),
            Some(c) => (now, c.epoch + 1),
            None => (now, 1),
        };
        let lease = TraderLease {
            holder: self.cfg.holder.clone(),
            host: self.cfg.host.clone(),
            pid: self.cfg.pid,
            acquired_at_ns,
            expires_at_ns: now.saturating_add(duration_ns(self.cfg.ttl)),
            epoch,
        };
        write_atomic(&self.cfg.path, &lease)?;
        *self.ours.lock() = Some(lease.clone());
        Ok(LeaseStatus::Held(lease))
    }

    /// Give the lease up (expire it now) if we hold it.
    pub fn release(&self) -> io::Result<()> {
        let _lock = self.flock()?;
        let current = read_lease(&self.cfg.path)?;
        if let Some(mut cur) = current.filter(|c| self.is_ours(c)) {
            cur.expires_at_ns = self.clock.now_ns();
            write_atomic(&self.cfg.path, &cur)?;
        }
        *self.ours.lock() = None;
        Ok(())
    }

    /// True while our last acquire/renew is valid (minus the safety margin).
    pub fn holds(&self) -> bool {
        let now = self.clock.now_ns();
        self.ours
            .lock()
            .as_ref()
            .is_some_and(|l| now < l.expires_at_ns.saturating_sub(duration_ns(SAFETY_MARGIN)))
    }

    /// Our current lease, if any.
    pub fn ours(&self) -> Option<TraderLease> {
        self.ours.lock().clone()
    }

    /// The lease on disk, whoever holds it.
    pub fn on_disk(&self) -> io::Result<Option<TraderLease>> {
        read_lease(&self.cfg.path)
    }

    /// Our holder name.
    pub fn holder(&self) -> &str {
        &self.cfg.holder
    }

    fn flock(&self) -> io::Result<File> {
        if let Some(dir) = self.cfg.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
        f.lock_exclusive()?;
        Ok(f) // unlocked when dropped
    }
}

fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{ext}"));
    path.with_file_name(name)
}

/// Read the lease; a missing or unparseable file counts as no lease (it can be taken over).
fn read_lease(path: &Path) -> io::Result<Option<TraderLease>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(susq_contracts::from_json::<TraderLease>(&text).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_atomic(path: &Path, lease: &TraderLease) -> io::Result<()> {
    let tmp = sidecar(path, "tmp");
    let text = susq_contracts::to_json(lease).map_err(io::Error::other)?;
    {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // Make the rename durable; a failure here only weakens durability, not correctness.
    if let Some(Ok(d)) = path.parent().map(File::open) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_core::ManualClock;

    const S: i64 = 1_000_000_000;

    fn lease(dir: &Path, holder: &str, pid: u32, clock: &ManualClock) -> Lease {
        Lease::new(
            LeaseConfig {
                path: dir.join("ops/state/trader.lease"),
                ttl: Duration::from_secs(30),
                holder: holder.into(),
                host: "oci-a1".into(),
                pid,
            },
            Arc::new(clock.clone()),
        )
    }

    #[test]
    fn exclusive_until_expiry_then_taken_over_with_new_epoch() {
        let dir = tempfile::tempdir().expect("tmp");
        let clock = ManualClock::at_ns(1_000 * S);
        let bridge = lease(dir.path(), "bridge", 11, &clock);
        let rust = lease(dir.path(), "rust-live", 22, &clock);

        let LeaseStatus::Held(b) = bridge.acquire().expect("io") else {
            panic!("bridge should get it")
        };
        assert_eq!(b.epoch, 1);
        assert!(matches!(rust.acquire().expect("io"), LeaseStatus::HeldBy(l) if l.holder == "bridge"));
        assert!(!rust.holds());

        // Renewal keeps the epoch and acquired_at.
        clock.advance(Duration::from_secs(10));
        let LeaseStatus::Held(b2) = bridge.acquire().expect("io") else {
            panic!("renew")
        };
        assert_eq!((b2.epoch, b2.acquired_at_ns), (1, b.acquired_at_ns));

        // Bridge dies: after expiry rust takes over and the epoch increments.
        clock.advance(Duration::from_secs(31));
        assert!(!bridge.holds());
        let LeaseStatus::Held(r) = rust.acquire().expect("io") else {
            panic!("takeover")
        };
        assert_eq!((r.holder.as_str(), r.epoch), ("rust-live", 2));
        assert!(rust.holds());
        // The old holder's renewal now fails.
        assert!(matches!(bridge.acquire().expect("io"), LeaseStatus::HeldBy(_)));
    }

    #[test]
    fn release_hands_over_immediately_and_margin_applies() {
        let dir = tempfile::tempdir().expect("tmp");
        let clock = ManualClock::at_ns(1_000 * S);
        let a = lease(dir.path(), "rust-live", 1, &clock);
        let b = lease(dir.path(), "bridge", 2, &clock);
        a.acquire().expect("io");
        clock.advance(Duration::from_secs(29));
        assert!(!a.holds(), "within the safety margin we stop trusting it");
        a.release().expect("io");
        assert!(matches!(b.acquire().expect("io"), LeaseStatus::Held(_)));
    }

    #[test]
    fn file_format_is_the_shared_contract() {
        let dir = tempfile::tempdir().expect("tmp");
        let clock = ManualClock::at_ns(1_791_475_200 * S);
        let a = lease(dir.path(), "rust-live", 4242, &clock);
        a.acquire().expect("io");
        let text = std::fs::read_to_string(dir.path().join("ops/state/trader.lease")).expect("read");
        assert!(text.contains("\"holder\": \"rust-live\""), "{text}");
        assert!(
            text.contains("\"expiresAtNs\": \"1791475230000000000\""),
            "{text}"
        );
        // A garbage lease file can be taken over (it protects nothing).
        std::fs::write(dir.path().join("ops/state/trader.lease"), "{not json").expect("write");
        let b = lease(dir.path(), "bridge", 1, &clock);
        assert!(matches!(b.acquire().expect("io"), LeaseStatus::Held(_)));
    }
}
