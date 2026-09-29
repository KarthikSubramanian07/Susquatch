//! Append-only request/response journal (Competition Data, never committed).
//!
//! One `susq.v1.JournalEntry` per line, compact canonical proto3 JSON (no whitespace), in
//! `journal/YYYY-MM-DD/<producer>.journal.jsonl`, rotated on the UTC day of each entry's `ts_ns`.
//! This is the same format the Python bridge writes, so both producers can be read by one tool.
//!
//! Producers never block on disk: [`Journal`] is a cheap handle over a bounded channel, and a
//! dedicated OS thread ([`JournalWriter`]) owns the file. [`Journal::append`] applies backpressure
//! when the channel is full; [`Journal::try_append`] never waits and counts what it drops.
//!
//! Secrets (the API key) are scrubbed from every string field before the line is written.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use chrono::{TimeZone, Utc};
use susq_contracts::v1::JournalEntry;
use tokio::sync::{mpsc, oneshot};

/// Placeholder written instead of a secret.
pub const REDACTED: &str = "[REDACTED]";

/// The writer thread is gone (shut down or crashed); nothing more can be journaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("journal writer is closed")]
pub struct JournalClosed;

enum Cmd {
    Entry(Box<JournalEntry>),
    Flush(oneshot::Sender<io::Result<()>>),
}

/// Producer-side handle. Clone freely.
#[derive(Clone)]
pub struct Journal {
    tx: mpsc::Sender<Cmd>,
    producer: Arc<str>,
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Journal")
            .field("producer", &self.producer)
            .field("dropped", &self.dropped())
            .finish()
    }
}

/// Where and how the writer thread writes.
#[derive(Debug, Clone)]
pub struct JournalConfig {
    /// Root directory (`journal/`).
    pub dir: PathBuf,
    /// Producer name, also the file stem (`rust-live`).
    pub producer: String,
    /// Channel capacity: the bound on in-memory backlog.
    pub capacity: usize,
    /// Strings that must never reach disk (API key, realtime token).
    pub redact: Vec<String>,
}

/// Owns the receiving end and the open file. Run it with [`JournalWriter::spawn`].
pub struct JournalWriter {
    rx: mpsc::Receiver<Cmd>,
    dir: PathBuf,
    producer: Arc<str>,
    redact: Vec<String>,
    open: Option<(String, BufWriter<File>)>,
    written: u64,
}

impl Journal {
    /// Create the handle and its (not yet running) writer.
    pub fn new(cfg: JournalConfig) -> (Journal, JournalWriter) {
        let (tx, rx) = mpsc::channel(cfg.capacity.max(1));
        let producer: Arc<str> = Arc::from(cfg.producer.as_str());
        let redact = cfg.redact.into_iter().filter(|s| s.len() >= 4).collect();
        let journal = Journal {
            tx,
            producer: producer.clone(),
            dropped: Arc::default(),
        };
        let writer = JournalWriter {
            rx,
            dir: cfg.dir,
            producer,
            redact,
            open: None,
            written: 0,
        };
        (journal, writer)
    }

    /// Create and start the writer thread in one step.
    pub fn start(cfg: JournalConfig) -> io::Result<(Journal, thread::JoinHandle<io::Result<u64>>)> {
        let (journal, writer) = Self::new(cfg);
        Ok((journal, writer.spawn()?))
    }

    /// A journal that accepts and discards everything (for tools and tests that don't journal).
    pub fn discard() -> Journal {
        let (journal, mut writer) = Self::new(JournalConfig {
            dir: PathBuf::new(),
            producer: "discard".into(),
            capacity: 1024,
            redact: vec![],
        });
        // A detached drain thread; it exits when the last handle is dropped.
        let _ = thread::Builder::new()
            .name("journal-discard".into())
            .spawn(move || {
                while let Some(cmd) = writer.rx.blocking_recv() {
                    if let Cmd::Flush(done) = cmd {
                        let _ = done.send(Ok(()));
                    }
                }
            });
        journal
    }

    /// Producer name stamped on entries that don't carry one.
    pub fn producer(&self) -> &str {
        &self.producer
    }

    /// Entries dropped by [`Journal::try_append`] because the backlog was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Append, waiting for room if the backlog is full (bounded backpressure).
    pub async fn append(&self, entry: JournalEntry) -> Result<(), JournalClosed> {
        self.tx
            .send(Cmd::Entry(Box::new(entry)))
            .await
            .map_err(|_| JournalClosed)
    }

    /// Append without waiting. Returns `false` (and counts a drop) if the backlog is full.
    pub fn try_append(&self, entry: JournalEntry) -> bool {
        match self.tx.try_send(Cmd::Entry(Box::new(entry))) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Wait until everything appended before this call is written and synced to disk.
    pub async fn flush(&self) -> Result<(), JournalClosed> {
        let (done, wait) = oneshot::channel();
        self.tx.send(Cmd::Flush(done)).await.map_err(|_| JournalClosed)?;
        match wait.await {
            Ok(Ok(())) => Ok(()),
            _ => Err(JournalClosed),
        }
    }
}

impl JournalWriter {
    /// Run on a dedicated OS thread until every [`Journal`] handle is dropped.
    /// The thread returns the number of lines written.
    pub fn spawn(self) -> io::Result<thread::JoinHandle<io::Result<u64>>> {
        thread::Builder::new()
            .name("journal-writer".into())
            .spawn(move || self.run())
    }

    fn run(mut self) -> io::Result<u64> {
        while let Some(cmd) = self.rx.blocking_recv() {
            self.handle(cmd)?;
            // Drain whatever is already queued, then flush once for the batch.
            while let Ok(cmd) = self.rx.try_recv() {
                self.handle(cmd)?;
            }
            if let Some((_, w)) = self.open.as_mut() {
                w.flush()?;
            }
        }
        if let Some((_, mut w)) = self.open.take() {
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        Ok(self.written)
    }

    fn handle(&mut self, cmd: Cmd) -> io::Result<()> {
        match cmd {
            Cmd::Entry(entry) => self.write_entry(*entry),
            Cmd::Flush(done) => {
                let res = self.sync();
                let _ = done.send(
                    res.as_ref()
                        .map(|_| ())
                        .map_err(|e| io::Error::new(e.kind(), e.to_string())),
                );
                // A failed sync is reported to the caller; the writer keeps going.
                Ok(())
            }
        }
    }

    fn sync(&mut self) -> io::Result<()> {
        if let Some((_, w)) = self.open.as_mut() {
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        Ok(())
    }

    fn write_entry(&mut self, mut entry: JournalEntry) -> io::Result<()> {
        if entry.producer.is_empty() {
            entry.producer = self.producer.to_string();
        }
        scrub(&mut entry, &self.redact);
        let day = utc_day(entry.ts_ns);
        let line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        let writer = self.writer_for(&day)?;
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        self.written += 1;
        Ok(())
    }

    fn writer_for(&mut self, day: &str) -> io::Result<&mut BufWriter<File>> {
        if self.open.as_ref().is_none_or(|(d, _)| d != day) {
            if let Some((_, mut old)) = self.open.take() {
                old.flush()?;
                old.get_ref().sync_data()?;
            }
            let path = journal_path(&self.dir, day, &self.producer);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.open = Some((day.to_owned(), BufWriter::new(file)));
        }
        // Just set above if it was missing.
        self.open
            .as_mut()
            .map(|(_, w)| w)
            .ok_or_else(|| io::Error::other("journal file not open"))
    }
}

/// `journal/YYYY-MM-DD/<producer>.journal.jsonl`
pub fn journal_path(dir: &Path, day: &str, producer: &str) -> PathBuf {
    dir.join(day).join(format!("{producer}.journal.jsonl"))
}

/// UTC calendar day (`YYYY-MM-DD`) of a nanosecond timestamp.
pub fn utc_day(ts_ns: i64) -> String {
    Utc.timestamp_nanos(ts_ns).format("%Y-%m-%d").to_string()
}

/// Replace every occurrence of a secret in the entry's free-text fields.
pub fn scrub(entry: &mut JournalEntry, secrets: &[String]) {
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        for field in [
            &mut entry.path,
            &mut entry.body_json,
            &mut entry.request_id,
            &mut entry.correlation_id,
            &mut entry.method,
            &mut entry.direction,
        ] {
            if field.contains(s.as_str()) {
                *field = field.replace(s.as_str(), REDACTED);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY1_NS: i64 = 1_791_504_000_000_000_000; // 2026-10-09T00:00:00Z

    fn entry(ts_ns: i64, body: &str) -> JournalEntry {
        JournalEntry {
            ts_ns,
            direction: "request".into(),
            method: "POST".into(),
            path: "/api/v1/orders".into(),
            body_json: body.into(),
            ..Default::default()
        }
    }

    fn cfg(dir: &Path, capacity: usize) -> JournalConfig {
        JournalConfig {
            dir: dir.to_owned(),
            producer: "rust-live".into(),
            capacity,
            redact: vec!["sk-test-secret".into()],
        }
    }

    fn read_lines(path: &Path) -> Vec<JournalEntry> {
        fs::read_to_string(path)
            .expect("journal file")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid JournalEntry line"))
            .collect()
    }

    #[tokio::test]
    async fn writes_compact_lines_and_rotates_on_utc_day() {
        let dir = tempfile::tempdir().expect("tmp");
        let (j, handle) = Journal::start(cfg(dir.path(), 16)).expect("start");
        j.append(entry(DAY1_NS - 1, "{\"a\":1}")).await.expect("append");
        j.append(entry(DAY1_NS, "{\"b\":2}")).await.expect("append");
        j.append(entry(DAY1_NS + 5, "{\"c\":3}")).await.expect("append");
        j.flush().await.expect("flush");
        drop(j);
        assert_eq!(handle.join().expect("thread").expect("io"), 3);

        let before = journal_path(dir.path(), "2026-10-08", "rust-live");
        let after = journal_path(dir.path(), "2026-10-09", "rust-live");
        assert_eq!(read_lines(&before).len(), 1);
        let day2 = read_lines(&after);
        assert_eq!(day2.len(), 2);
        assert_eq!(day2[0].producer, "rust-live");
        let raw = fs::read_to_string(&after).expect("read");
        let first = raw.lines().next().expect("line");
        assert!(
            !first.contains(": ") && !first.contains(", "),
            "compact JSON: {first}"
        );
        assert!(
            first.contains("\"tsNs\":\"1791504000000000000\""),
            "canonical proto3 JSON: {first}"
        );
    }

    #[tokio::test]
    async fn never_writes_the_api_key() {
        let dir = tempfile::tempdir().expect("tmp");
        let (j, handle) = Journal::start(cfg(dir.path(), 4)).expect("start");
        let mut e = entry(DAY1_NS, "{\"echo\":\"Bearer sk-test-secret\"}");
        e.path = "/api/v1/orders?key=sk-test-secret".into();
        j.append(e).await.expect("append");
        drop(j);
        handle.join().expect("thread").expect("io");
        let raw = fs::read_to_string(journal_path(dir.path(), "2026-10-09", "rust-live")).expect("read");
        assert!(!raw.contains("sk-test-secret"));
        assert!(raw.contains(REDACTED));
    }

    #[tokio::test]
    async fn try_append_is_bounded_and_counts_drops() {
        let dir = tempfile::tempdir().expect("tmp");
        let (j, writer) = Journal::new(cfg(dir.path(), 2));
        // Writer not running yet: the backlog fills up and the rest is dropped, never blocking.
        assert!(j.try_append(entry(DAY1_NS, "1")));
        assert!(j.try_append(entry(DAY1_NS, "2")));
        assert!(!j.try_append(entry(DAY1_NS, "3")));
        assert_eq!(j.dropped(), 1);
        let handle = writer.spawn().expect("spawn");
        // append() waits for room instead of dropping.
        for i in 0..10 {
            j.append(entry(DAY1_NS, &i.to_string())).await.expect("append");
        }
        drop(j);
        assert_eq!(handle.join().expect("thread").expect("io"), 12);
    }

    #[tokio::test]
    async fn closed_writer_is_reported() {
        let dir = tempfile::tempdir().expect("tmp");
        let (j, writer) = Journal::new(cfg(dir.path(), 2));
        drop(writer);
        assert_eq!(j.append(entry(DAY1_NS, "x")).await, Err(JournalClosed));
        assert!(!j.try_append(entry(DAY1_NS, "x")));
    }

    #[tokio::test]
    async fn discard_accepts_everything() {
        let j = Journal::discard();
        for _ in 0..5000 {
            j.append(entry(DAY1_NS, "x")).await.expect("append");
        }
        j.flush().await.expect("flush");
    }

    #[test]
    fn utc_day_boundaries() {
        assert_eq!(utc_day(DAY1_NS), "2026-10-09");
        assert_eq!(utc_day(DAY1_NS - 1), "2026-10-08");
    }
}
