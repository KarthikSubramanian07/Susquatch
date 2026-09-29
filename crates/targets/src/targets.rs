//! A validated target file with lookups by exchange id.

use std::collections::HashMap;

use susq_contracts::v1::{FairValue, TargetFile, TargetPosition};

/// A target file that passed every structural and content check, indexed for lookups.
///
/// Freshness is not part of this type: the same value can go stale over time, which
/// [`crate::TargetsReader::increase_exposure_allowed`] tracks.
#[derive(Debug, Clone, PartialEq)]
pub struct Targets {
    file: TargetFile,
    source: String,
    positions: HashMap<String, usize>,
    fair: HashMap<String, usize>,
}

impl Targets {
    /// Index a file whose exchange ids are already known to be non-empty and unique.
    pub(crate) fn new(file: TargetFile, source: String) -> Self {
        let positions = index(file.positions.iter().map(|p| p.exchange_id.as_str()));
        let fair = index(file.fair.iter().map(|f| f.exchange_id.as_str()));
        Self {
            file,
            source,
            positions,
            fair,
        }
    }

    /// The underlying file exactly as parsed.
    pub fn file(&self) -> &TargetFile {
        &self.file
    }

    /// File name it was loaded from (`targets-<utc>-<hash8>.json`, or `current.json` in direct form).
    pub fn source(&self) -> &str {
        &self.source
    }

    /// `header.produced_at_ns` (UTC nanoseconds).
    pub fn produced_at_ns(&self) -> i64 {
        self.file.header.as_ref().map_or(0, |h| h.produced_at_ns)
    }

    /// `valid_until_ns` (UTC nanoseconds).
    pub fn valid_until_ns(&self) -> i64 {
        self.file.valid_until_ns
    }

    /// Target position for a Cup market, if the file has one.
    pub fn target_for(&self, exchange_id: &str) -> Option<&TargetPosition> {
        self.positions.get(exchange_id).map(|&i| &self.file.positions[i])
    }

    /// Fair value for a Cup market, if the file has one.
    pub fn fair_for(&self, exchange_id: &str) -> Option<&FairValue> {
        self.fair.get(exchange_id).map(|&i| &self.file.fair[i])
    }

    /// Exchange ids of positions strategies may not sell, in file order.
    pub fn locked_exchanges(&self) -> impl Iterator<Item = &str> {
        self.file
            .positions
            .iter()
            .filter(|p| p.locked)
            .map(|p| p.exchange_id.as_str())
    }
}

fn index<'a>(ids: impl Iterator<Item = &'a str>) -> HashMap<String, usize> {
    ids.enumerate().map(|(i, id)| (id.to_owned(), i)).collect()
}
