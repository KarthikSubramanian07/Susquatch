//! Write-ahead intent log: `state/<producer>/intents.jsonl`, one `susq.v1.OrderIntent` per line
//! (compact proto3 JSON), last line per `intent_id` wins.
//!
//! Every state change is appended and `fsync`ed before the caller proceeds, so an intent is on disk
//! before its request is on the wire. On open the log is replayed and compacted (atomic tmp + rename)
//! to the latest line of each intent still worth remembering.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use susq_contracts::v1::{IntentState, OrderIntent};

/// How long closed intents are remembered (so a re-decided intent id is still refused).
pub const RETAIN_CLOSED_NS: i64 = 48 * 3600 * 1_000_000_000;

/// The append-only intent log.
#[derive(Debug)]
pub struct IntentStore {
    path: PathBuf,
    file: File,
    /// Lines that failed to parse during the last open (corruption is reported, never fatal).
    pub corrupt_lines: usize,
}

impl IntentStore {
    /// Open (creating if needed), replay, compact, and return the latest state of each intent.
    pub fn open(path: &Path, now_ns: i64) -> io::Result<(Self, BTreeMap<String, OrderIntent>)> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let (latest, corrupt_lines) = replay(path)?;
        let kept: BTreeMap<String, OrderIntent> = latest
            .into_iter()
            .filter(|(_, i)| worth_keeping(i, now_ns))
            .collect();
        compact(path, kept.values())?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok((
            Self {
                path: path.to_owned(),
                file,
                corrupt_lines,
            },
            kept,
        ))
    }

    /// Durably append one intent state (write + fsync).
    pub fn append(&mut self, intent: &OrderIntent) -> io::Result<()> {
        let line = serde_json::to_string(intent).map_err(io::Error::other)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.sync_data()
    }

    /// Log location.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn is_closed(i: &OrderIntent) -> bool {
    matches!(
        IntentState::try_from(i.state),
        Ok(IntentState::Done | IntentState::Rejected)
    )
}

fn worth_keeping(i: &OrderIntent, now_ns: i64) -> bool {
    !is_closed(i) || now_ns.saturating_sub(i.updated_at_ns) < RETAIN_CLOSED_NS
}

fn replay(path: &Path) -> io::Result<(BTreeMap<String, OrderIntent>, usize)> {
    let mut latest = BTreeMap::new();
    let mut corrupt = 0;
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((latest, 0)),
        Err(e) => return Err(e),
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<OrderIntent>(&line) {
            Ok(i) if !i.intent_id.is_empty() => {
                latest.insert(i.intent_id.clone(), i);
            }
            _ => {
                // A torn last line after a crash, or garbage: skip it, count it.
                corrupt += 1;
                tracing::warn!(path = %path.display(), "skipping unreadable intent line");
            }
        }
    }
    Ok((latest, corrupt))
}

fn compact<'a>(path: &Path, intents: impl Iterator<Item = &'a OrderIntent>) -> io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = File::create(&tmp)?;
        for i in intents {
            f.write_all(serde_json::to_string(i).map_err(io::Error::other)?.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // Make the rename itself durable.
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(id: &str, state: IntentState, updated: i64) -> OrderIntent {
        OrderIntent {
            intent_id: id.into(),
            state: state as i32,
            updated_at_ns: updated,
            ..Default::default()
        }
    }

    #[test]
    fn last_line_wins_and_survives_reopen() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("rust-live/intents.jsonl");
        let (mut s, loaded) = IntentStore::open(&path, 0).expect("open");
        assert!(loaded.is_empty());
        s.append(&intent("a", IntentState::Pending, 1)).expect("append");
        s.append(&intent("a", IntentState::Sent, 2)).expect("append");
        s.append(&intent("b", IntentState::Pending, 3)).expect("append");
        drop(s);
        let (_, loaded) = IntentStore::open(&path, 10).expect("reopen");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["a"].state, IntentState::Sent as i32);
    }

    #[test]
    fn torn_lines_are_skipped_and_counted() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("intents.jsonl");
        let good = serde_json::to_string(&intent("a", IntentState::Acked, 1)).expect("json");
        fs::write(&path, format!("{good}\n{{\"intentId\":\"b\",\"sta")).expect("write");
        let (s, loaded) = IntentStore::open(&path, 2).expect("open");
        assert_eq!(s.corrupt_lines, 1);
        assert_eq!(loaded.len(), 1);
        // Compaction removed the torn line.
        assert_eq!(fs::read_to_string(&path).expect("read").lines().count(), 1);
    }

    #[test]
    fn old_closed_intents_are_compacted_away() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("intents.jsonl");
        let (mut s, _) = IntentStore::open(&path, 0).expect("open");
        s.append(&intent("old-done", IntentState::Done, 0))
            .expect("append");
        s.append(&intent("recent-done", IntentState::Done, RETAIN_CLOSED_NS))
            .expect("append");
        s.append(&intent("old-unknown", IntentState::Unknown, 0))
            .expect("append");
        drop(s);
        let (_, loaded) = IntentStore::open(&path, RETAIN_CLOSED_NS + 1).expect("reopen");
        assert!(!loaded.contains_key("old-done"));
        assert!(loaded.contains_key("recent-done"));
        assert!(
            loaded.contains_key("old-unknown"),
            "unresolved intents are never forgotten"
        );
    }
}
