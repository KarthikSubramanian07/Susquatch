//! Random byte corruption of a valid target file must never load different content.

use std::{fs, path::Path, time::Duration};

use proptest::prelude::*;
use susq_contracts::{
    from_json,
    v1::{TargetFile, TargetPointer},
};
use susq_targets::{ReaderConfig, RefreshOutcome, TargetsReader};

/// One hour after the golden fixture's `produced_at` (2026-10-08T16:00:00Z).
const NOW: i64 = 1_791_478_800_000_000_000;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures")
        .join(name);
    fs::read_to_string(path).expect("fixture")
}

/// Load `file_bytes` behind the golden pointer in a fresh directory and reader.
fn load(pointer: &str, file_name: &str, file_bytes: &[u8]) -> RefreshOutcome {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("current.json"), pointer).expect("pointer");
    fs::write(dir.path().join(file_name), file_bytes).expect("file");
    let cfg = ReaderConfig::new(dir.path(), Duration::from_secs(36 * 3_600), "midterm-elections");
    TargetsReader::new(cfg).refresh(NOW)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn corruption_is_rejected_or_a_semantic_no_op(
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
    ) {
        let pointer = fixture("target_pointer.json");
        let file_name = from_json::<TargetPointer>(&pointer).expect("pointer").file;
        let text = fixture("target_file.json");
        let original: TargetFile = from_json(&text).expect("file");

        let mut bytes = text.into_bytes();
        for (at, byte) in &edits {
            let i = at.index(bytes.len());
            bytes[i] = *byte;
        }

        match load(&pointer, &file_name, &bytes) {
            RefreshOutcome::Loaded(t) => prop_assert_eq!(t.file(), &original),
            RefreshOutcome::Rejected(_) => {}
            RefreshOutcome::Unchanged => prop_assert!(false, "a fresh reader cannot be unchanged"),
        }
    }
}

#[test]
fn uncorrupted_fixture_loads() {
    let pointer = fixture("target_pointer.json");
    let file_name = from_json::<TargetPointer>(&pointer).expect("pointer").file;
    let outcome = load(&pointer, &file_name, fixture("target_file.json").as_bytes());
    assert!(matches!(outcome, RefreshOutcome::Loaded(_)), "{outcome:?}");
}

#[test]
fn whitespace_changes_load_identical_content() {
    let pointer = fixture("target_pointer.json");
    let file_name = from_json::<TargetPointer>(&pointer).expect("pointer").file;
    let text = fixture("target_file.json");
    let original: TargetFile = from_json(&text).expect("file");
    let reflowed = text.replace("\n  ", "\n\t ");
    assert_ne!(reflowed, text);
    match load(&pointer, &file_name, reflowed.as_bytes()) {
        RefreshOutcome::Loaded(t) => assert_eq!(t.file(), &original),
        other => panic!("whitespace must not matter: {other:?}"),
    }
}
