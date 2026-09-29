//! On-disk capture archive: `data/enr/<source>/<YYYY-MM-DD>/` holding
//! * `raw/<HHMMSS.mmm>-<n>-<slug>`: every *changed* response body, byte for byte;
//! * `batches.jsonl`: every emitted `CountyResultsBatch` (compact proto3 JSON, one per line);
//! * `events.jsonl`: rejections, held/applied corrections and disputes.
//!
//! This is what we run during the CA SoS test windows to build real replay data. Dates are UTC.
//! `data/` is gitignored.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use susq_contracts::v1::CountyResultsBatch;

use crate::adapters::RawPayload;
use crate::pipeline::PipelineEvent;

/// Writer for one capture root.
#[derive(Debug)]
pub struct Archive {
    root: PathBuf,
    last_raw: HashMap<String, [u8; 32]>,
    counter: u64,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            last_raw: HashMap::new(),
            counter: 0,
        }
    }

    /// Directory for `source` on the UTC date of `at_ns`.
    pub fn day_dir(&self, source: &str, at_ns: i64) -> PathBuf {
        let date = chrono::DateTime::from_timestamp_nanos(at_ns)
            .format("%Y-%m-%d")
            .to_string();
        self.root.join(sanitize(source)).join(date)
    }

    /// Store a raw body unless it is byte-identical to the last one stored for the same URL.
    /// Returns the path written, if any.
    pub fn write_raw(
        &mut self,
        source: &str,
        payload: &RawPayload,
        at_ns: i64,
    ) -> std::io::Result<Option<PathBuf>> {
        let digest: [u8; 32] = Sha256::digest(&payload.body).into();
        if self.last_raw.get(&payload.url) == Some(&digest) {
            return Ok(None);
        }
        let dir = self.day_dir(source, at_ns).join("raw");
        fs::create_dir_all(&dir)?;
        let ts = chrono::DateTime::from_timestamp_nanos(at_ns)
            .format("%H%M%S%.3f")
            .to_string();
        self.counter += 1;
        let path = dir.join(format!("{ts}-{}-{}", self.counter, url_slug(&payload.url)));
        write_atomic(&path, &payload.body)?;
        self.last_raw.insert(payload.url.clone(), digest);
        Ok(Some(path))
    }

    /// Append one batch as a JSON line.
    pub fn append_batch(&self, source: &str, batch: &CountyResultsBatch, at_ns: i64) -> std::io::Result<()> {
        let line = serde_json::to_string(batch).map_err(std::io::Error::other)?;
        self.append_line(&self.day_dir(source, at_ns).join("batches.jsonl"), &line)
    }

    /// Append pipeline events (as debug text with a timestamp) for later review.
    pub fn append_events(&self, source: &str, events: &[PipelineEvent], at_ns: i64) -> std::io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let path = self.day_dir(source, at_ns).join("events.jsonl");
        for e in events {
            let line = serde_json::json!({"at_ns": at_ns, "event": format!("{e:?}")}).to_string();
            self.append_line(&path, &line)?;
        }
        Ok(())
    }

    fn append_line(&self, path: &Path, line: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

/// Path-safe version of a source id (`clarity:GA` → `clarity_GA`).
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Short, readable file name from a URL: the last two path segments.
fn url_slug(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let segs: Vec<&str> = path.trim_end_matches('/').rsplit('/').take(2).collect();
    let slug: String = segs.into_iter().rev().collect::<Vec<_>>().join("_");
    let mut s = sanitize(&slug).replace("__", "_");
    s.truncate(80);
    if s.is_empty() { "payload".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn raw_bodies_are_deduped_per_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut a = Archive::new(dir.path());
        let p = RawPayload {
            url: "https://x.invalid/GA/1/summary.json".into(),
            body: Bytes::from_static(b"{}"),
        };
        let at = 1_793_754_123_000_000_000;
        assert!(a.write_raw("clarity:GA", &p, at).expect("write").is_some());
        assert!(a.write_raw("clarity:GA", &p, at).expect("write").is_none());
        let changed = RawPayload {
            body: Bytes::from_static(b"{\"v\":1}"),
            ..p
        };
        let path = a
            .write_raw("clarity:GA", &changed, at)
            .expect("write")
            .expect("written");
        assert!(path.starts_with(dir.path().join("clarity_GA").join("2026-11-04").join("raw")));
        assert!(path.to_string_lossy().ends_with("1_summary_json"));
    }

    #[test]
    fn batches_append_as_json_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = Archive::new(dir.path());
        let b = CountyResultsBatch::default();
        a.append_batch("ca_sos", &b, 0).expect("append");
        a.append_batch("ca_sos", &b, 0).expect("append");
        let text = fs::read_to_string(dir.path().join("ca_sos/1970-01-01/batches.jsonl")).expect("read");
        assert_eq!(text.lines().count(), 2);
    }
}
