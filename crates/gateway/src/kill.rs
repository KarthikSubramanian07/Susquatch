//! Kill switch: `ops/KILL` file, the in-process flag, and NATS `ops.kill` (which sets the flag).
//!
//! Kill beats everything: the gateway checks it before every write. The in-process flag is sticky
//! until restart (a NATS or dashboard kill must not be undone by a flaky file system); the file is
//! human-controlled, so removing it lifts a file kill.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;

/// Shared kill state. Clones share the flag.
#[derive(Debug, Clone)]
pub struct KillSwitch {
    file: PathBuf,
    flag: Arc<Mutex<Option<String>>>,
}

impl KillSwitch {
    /// Watch `file` (typically `ops/KILL`).
    pub fn new(file: impl Into<PathBuf>) -> Self {
        Self {
            file: file.into(),
            flag: Arc::default(),
        }
    }

    /// Kill in process (NATS `ops.kill`, SIGTERM handling, a breaker). Sticky until restart.
    pub fn trigger(&self, reason: impl Into<String>) {
        let mut flag = self.flag.lock();
        if flag.is_none() {
            let reason = reason.into();
            tracing::error!(%reason, "kill switch triggered");
            *flag = Some(reason);
        }
    }

    /// The kill reason if killed by either source.
    pub fn reason(&self) -> Option<String> {
        if let Some(r) = self.flag.lock().clone() {
            return Some(r);
        }
        read_kill_file(&self.file)
    }

    /// True when killed.
    pub fn is_killed(&self) -> bool {
        self.reason().is_some()
    }

    /// The watched file.
    pub fn file(&self) -> &Path {
        &self.file
    }
}

/// `Some(reason)` when the kill file exists. Unreadable contents still count as a kill.
fn read_kill_file(path: &Path) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let reason: String = text.trim().chars().take(200).collect();
            Some(if reason.is_empty() {
                format!("{} present", path.display())
            } else {
                reason
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        // Exists but unreadable (permissions, it's a directory, ...): fail safe.
        Err(e) => Some(format!("{} unreadable: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_and_flag() {
        let dir = tempfile::tempdir().expect("tmp");
        let file = dir.path().join("KILL");
        let k = KillSwitch::new(&file);
        assert!(!k.is_killed());
        std::fs::write(&file, "dashboard kill\n").expect("write");
        assert_eq!(k.reason().as_deref(), Some("dashboard kill"));
        std::fs::remove_file(&file).expect("rm");
        assert!(!k.is_killed(), "removing the file lifts a file kill");
        k.clone().trigger("ops.kill from watchdog");
        assert_eq!(k.reason().as_deref(), Some("ops.kill from watchdog"));
        k.trigger("second reason is ignored");
        assert_eq!(k.reason().as_deref(), Some("ops.kill from watchdog"));
    }

    #[test]
    fn empty_or_unreadable_file_still_kills() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("KILL"), "").expect("write");
        assert!(KillSwitch::new(dir.path().join("KILL")).is_killed());
        // A directory where the file should be is unreadable as a file.
        std::fs::create_dir(dir.path().join("KILLDIR")).expect("mkdir");
        assert!(KillSwitch::new(dir.path().join("KILLDIR")).is_killed());
    }
}
