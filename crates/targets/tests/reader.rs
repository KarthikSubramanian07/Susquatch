//! End-to-end behaviour of `TargetsReader` against real files in a temporary directory.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use susq_contracts::{
    SCHEMA_VERSION, target_content_sha256, to_json,
    v1::{
        ClusterLimit, FairValue, Header, Producer, TargetFile, TargetPointer, TargetPosition, TourneyEstimate,
    },
};
use susq_targets::{ContentError, Document, ReaderConfig, RefreshOutcome, RejectReason, TargetsReader};
use tempfile::TempDir;

const SLUG: &str = "midterm-elections";
const H: i64 = 3_600 * 1_000_000_000;
const MAX_AGE_H: u64 = 36;
/// 2026-10-08T16:00:00Z, the fixture's `produced_at`.
const T0: i64 = 1_791_475_200_000_000_000;

// ---------------------------------------------------------------- helpers

struct Env {
    dir: TempDir,
    reader: TargetsReader,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = ReaderConfig::new(dir.path(), Duration::from_secs(MAX_AGE_H * 3_600), SLUG);
        Self {
            reader: TargetsReader::new(cfg),
            dir,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write(&self, name: &str, text: &str) {
        // Write-then-rename, like the Python side, so a reader never sees a torn file.
        let tmp = self.path(&format!(".{name}.tmp"));
        fs::write(&tmp, text).expect("write");
        fs::rename(&tmp, self.path(name)).expect("rename");
    }

    fn write_file(&self, name: &str, tf: &TargetFile) {
        self.write(name, &to_json(tf).expect("json"));
    }

    fn point_to(&self, name: &str, hash: &str) {
        self.write_pointer(&pointer(name, hash));
    }

    fn write_pointer(&self, ptr: &TargetPointer) {
        self.write("current.json", &to_json(ptr).expect("json"));
    }

    /// Write `tf` under `name` and swap the pointer to it.
    fn publish(&self, name: &str, tf: &TargetFile) {
        self.write_file(name, tf);
        self.point_to(name, &tf.content_sha256);
    }

    fn refresh(&mut self, now: i64) -> RefreshOutcome {
        self.reader.refresh(now)
    }

    fn expect_loaded(&mut self, now: i64) -> susq_targets::Targets {
        match self.refresh(now) {
            RefreshOutcome::Loaded(t) => (*t).clone(),
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    fn expect_rejected(&mut self, now: i64) -> RejectReason {
        match self.refresh(now) {
            RefreshOutcome::Rejected(r) => r,
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}

fn header(produced_at_ns: i64) -> Header {
    Header {
        schema_version: SCHEMA_VERSION,
        produced_at_ns,
        producer: Some(Producer {
            name: "test".into(),
            git_sha: "abc123".into(),
        }),
        seq: 1,
    }
}

fn pointer(file: &str, hash: &str) -> TargetPointer {
    TargetPointer {
        header: Some(header(T0)),
        file: file.into(),
        content_sha256: hash.into(),
    }
}

/// Recompute and store `content_sha256`.
fn seal(mut tf: TargetFile) -> TargetFile {
    tf.content_sha256 = target_content_sha256(&tf);
    tf
}

/// A valid file produced at `produced`, valid for 24 h, with one unlocked and one locked position.
fn sample(produced: i64) -> TargetFile {
    seal(TargetFile {
        header: Some(header(produced)),
        tournament_slug: SLUG.into(),
        valid_until_ns: produced + 24 * H,
        fair: vec![FairValue {
            exchange_id: "101".into(),
            market_id: "101".into(),
            p_yes: 0.5,
            p_lo: 0.4,
            p_hi: 0.6,
            source: "test".into(),
        }],
        positions: vec![
            TargetPosition {
                exchange_id: "101".into(),
                market_id: "101".into(),
                target_qty: 10,
                max_entry_price: 0.55,
                ..Default::default()
            },
            TargetPosition {
                exchange_id: "102".into(),
                market_id: "102".into(),
                target_qty: -5,
                max_entry_price: 0.3,
                locked: true,
                ..Default::default()
            },
        ],
        clusters: vec![ClusterLimit {
            cluster: "national".into(),
            max_gross_frac: 0.25,
        }],
        tourney: Some(TourneyEstimate {
            p_first: 0.03,
            p_top3: 0.08,
            ..Default::default()
        }),
        notes: "sample".into(),
        ..Default::default()
    })
}

fn edited(edit: impl FnOnce(&mut TargetFile)) -> TargetFile {
    let mut tf = sample(T0);
    edit(&mut tf);
    seal(tf)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures")
        .join(name)
}

/// Copy the golden pointer and the file it names into the env.
fn install_fixtures(env: &Env) -> TargetPointer {
    let ptr_text = fs::read_to_string(fixture("target_pointer.json")).expect("pointer fixture");
    let ptr: TargetPointer = susq_contracts::from_json(&ptr_text).expect("pointer parses");
    fs::copy(fixture("target_file.json"), env.path(&ptr.file)).expect("copy file");
    fs::copy(fixture("target_pointer.json"), env.path("current.json")).expect("copy pointer");
    ptr
}

// ---------------------------------------------------------------- happy paths

#[test]
fn golden_fixtures_load_with_lookups() {
    let mut env = Env::new();
    let ptr = install_fixtures(&env);
    let t = env.expect_loaded(T0 + H);

    assert_eq!(t.source(), ptr.file);
    assert_eq!(t.file().content_sha256, ptr.content_sha256);
    let leg = t.target_for("210").expect("position 210");
    assert_eq!((leg.target_qty, leg.locked), (-4321, true));
    assert_eq!(t.target_for("211").expect("position 211").target_qty, 987);
    assert!(
        t.target_for("381").is_none(),
        "381 has a fair value but no position"
    );
    assert!((t.fair_for("381").expect("fair 381").p_yes - 0.86).abs() < 1e-12);
    assert_eq!(t.fair_for("382").expect("edge zero").p_yes, 0.0);
    assert_eq!(t.locked_exchanges().collect::<Vec<_>>(), ["210"]);

    assert!(env.reader.increase_exposure_allowed());
    let health = env.reader.health(T0 + H);
    assert_eq!((health.name.as_str(), health.state.as_str()), ("targets", "ok"));
    assert_eq!(
        health.detail,
        format!("{} from 1.0h ago, valid for 35.0h", ptr.file)
    );
    assert_eq!(health.last_ok_ns, T0 + H);
}

#[test]
fn current_json_may_be_the_target_file_itself() {
    let mut env = Env::new();
    fs::copy(fixture("target_file.json"), env.path("current.json")).expect("copy");
    let t = env.expect_loaded(T0 + H);
    assert_eq!(t.source(), "current.json");
    assert_eq!(t.target_for("210").expect("210").target_qty, -4321);
}

#[cfg(unix)]
#[test]
fn current_json_may_be_a_symlink_to_the_target_file() {
    let mut env = Env::new();
    let name = "targets-20261008T160000Z-e6622703.json";
    fs::copy(fixture("target_file.json"), env.path(name)).expect("copy");
    std::os::unix::fs::symlink(env.path(name), env.path("current.json")).expect("symlink");
    env.expect_loaded(T0 + H);
    assert!(env.reader.increase_exposure_allowed());
}

// ---------------------------------------------------------------- every rejection reason

#[test]
fn nothing_loaded_is_down_and_blocks_increases() {
    let mut env = Env::new();
    assert!(!env.reader.increase_exposure_allowed());
    assert_eq!(env.reader.health(T0).state, "down");
    assert_eq!(env.reader.health(T0).detail, "no targets loaded: not read yet");

    let r = env.expect_rejected(T0);
    assert!(matches!(r, RejectReason::PointerMissing { .. }), "{r}");
    assert!(env.reader.current().is_none());
    assert!(!env.reader.increase_exposure_allowed());
    let health = env.reader.health(T0);
    assert_eq!(health.state, "down");
    assert!(
        health.detail.starts_with("no targets loaded: pointer "),
        "{}",
        health.detail
    );
    assert_eq!(health.last_ok_ns, 0);
}

#[test]
fn pointer_unreadable() {
    let mut env = Env::new();
    fs::create_dir(env.path("current.json")).expect("a directory where the pointer should be");
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::PointerUnreadable { .. }
    ));
}

#[test]
fn pointer_unparseable() {
    let mut env = Env::new();
    env.write("current.json", "{\"file\": ");
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::PointerUnparseable { .. }
    ));
    env.write("current.json", "{\"file\": \"x.json\", \"surprise\": 1}");
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::PointerUnparseable { .. }
    ));
}

#[test]
fn pointer_path_traversal_is_refused() {
    let mut env = Env::new();
    let tf = sample(T0);
    // A perfectly valid file in a sibling directory must still not be followed.
    let sibling = tempfile::tempdir().expect("sibling tempdir");
    let outside = sibling.path().join("escaped.json");
    fs::write(&outside, to_json(&tf).expect("json")).expect("write outside");
    let sibling_name = sibling.path().file_name().expect("name").to_string_lossy();
    for name in [
        format!("../{sibling_name}/escaped.json"),
        outside.display().to_string(),
        "sub/targets.json".to_owned(),
        "..".to_owned(),
    ] {
        env.point_to(&name, &tf.content_sha256);
        let r = env.expect_rejected(T0);
        assert_eq!(r, RejectReason::UnsafeFileName { name: name.clone() }, "{name:?}");
    }
    assert!(env.reader.current().is_none());
}

#[test]
fn pointer_with_empty_file_name_is_refused() {
    // proto3 JSON omits an empty `file`, so this reads as a bare TargetFile and fails its hash check.
    let mut env = Env::new();
    env.point_to("", &sample(T0).content_sha256);
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::ContentHashMismatch { .. }
    ));
}

#[test]
fn target_file_missing() {
    let mut env = Env::new();
    env.point_to("targets-nope.json", "00");
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::FileMissing {
            file: "targets-nope.json".into()
        }
    );
}

#[test]
fn target_file_unparseable() {
    let mut env = Env::new();
    env.write("t.json", "{\"header\": {}, \"notAField\": true}");
    env.point_to("t.json", "00");
    assert!(
        matches!(env.expect_rejected(T0), RejectReason::FileUnparseable { file, .. } if file == "t.json")
    );
    env.write("t.json", "not json");
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::FileUnparseable { .. }
    ));
}

#[test]
fn pointer_header_missing_or_wrong_schema() {
    let mut env = Env::new();
    let tf = sample(T0);
    env.write_file("t.json", &tf);

    let mut ptr = pointer("t.json", &tf.content_sha256);
    ptr.header = None;
    env.write_pointer(&ptr);
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::MissingHeader {
            doc: Document::Pointer
        }
    );

    let mut ptr = pointer("t.json", &tf.content_sha256);
    ptr.header.as_mut().expect("header").schema_version = SCHEMA_VERSION + 1;
    env.write_pointer(&ptr);
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::SchemaVersion {
            doc: Document::Pointer,
            got: SCHEMA_VERSION + 1,
            want: SCHEMA_VERSION
        }
    );
}

#[test]
fn file_header_missing_or_wrong_schema() {
    let mut env = Env::new();
    env.publish("a.json", &edited(|f| f.header = None));
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::MissingHeader {
            doc: Document::TargetFile
        }
    );

    env.publish(
        "b.json",
        &edited(|f| f.header.as_mut().expect("header").schema_version = 0),
    );
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::SchemaVersion {
            doc: Document::TargetFile,
            got: 0,
            want: SCHEMA_VERSION
        }
    );
}

#[test]
fn pointer_hash_mismatch() {
    let mut env = Env::new();
    let tf = sample(T0);
    env.write_file("t.json", &tf);
    env.point_to("t.json", &"0".repeat(64));
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::PointerHashMismatch {
            pointer: "0".repeat(64),
            file: tf.content_sha256
        }
    );
}

#[test]
fn content_hash_mismatch() {
    let mut env = Env::new();
    let mut tf = sample(T0);
    let declared = tf.content_sha256.clone();
    tf.positions[0].target_qty = 10_000; // tampered after sealing
    let computed = target_content_sha256(&tf);
    env.publish("t.json", &tf);
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::ContentHashMismatch { declared, computed }
    );
}

#[test]
fn wrong_tournament() {
    let mut env = Env::new();
    env.publish("t.json", &edited(|f| f.tournament_slug = "other-cup".into()));
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::WrongTournament {
            got: "other-cup".into(),
            want: SLUG.into()
        }
    );
}

#[test]
fn empty_fixture_is_refused() {
    let mut env = Env::new();
    fs::copy(fixture("target_file_empty.json"), env.path("current.json")).expect("copy");
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::WrongTournament { .. }
    ));
}

#[test]
fn expired() {
    let mut env = Env::new();
    env.publish("t.json", &edited(|f| f.valid_until_ns = T0 + H));
    assert!(matches!(
        env.expect_rejected(T0 + H),
        RejectReason::Expired { .. }
    ));
}

#[test]
fn too_old() {
    let mut env = Env::new();
    env.publish("t.json", &edited(|f| f.valid_until_ns = T0 + 100 * H));
    let r = env.expect_rejected(T0 + 37 * H);
    assert!(matches!(r, RejectReason::TooOld { .. }), "{r}");
}

#[test]
fn produced_in_future_heals_when_clock_catches_up() {
    let mut env = Env::new();
    let produced = T0 + H / 6; // ten minutes ahead of our clock
    env.publish("t.json", &sample(produced));
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::ProducedInFuture { .. }
    ));
    assert!(!env.reader.increase_exposure_allowed());
    // Six minutes later the file is only four minutes ahead, within the five-minute skew.
    env.expect_loaded(T0 + H / 10);
    assert!(env.reader.increase_exposure_allowed());
}

#[test]
fn invalid_content() {
    let mut env = Env::new();
    env.publish("t.json", &edited(|f| f.fair[0].p_hi = 0.45));
    assert!(matches!(
        env.expect_rejected(T0),
        RejectReason::InvalidContent(ContentError::BandOrder { .. })
    ));
    env.publish("u.json", &edited(|f| f.positions[1].exchange_id = "101".into()));
    assert_eq!(
        env.expect_rejected(T0),
        RejectReason::InvalidContent(ContentError::DuplicatePosition {
            exchange_id: "101".into()
        })
    );
}

// ---------------------------------------------------------------- lifecycle

#[test]
fn rejection_keeps_last_good_but_blocks_increases_until_recovery() {
    let mut env = Env::new();
    let good = sample(T0);
    env.publish("good.json", &good);
    env.expect_loaded(T0);
    assert!(env.reader.increase_exposure_allowed());

    let mut bad = sample(T0 + H);
    bad.positions[0].target_qty = 999; // hash no longer matches
    env.publish("bad.json", &bad);
    assert!(matches!(
        env.expect_rejected(T0 + H),
        RejectReason::ContentHashMismatch { .. }
    ));
    assert!(!env.reader.increase_exposure_allowed());
    let kept = env.reader.current().expect("last good kept");
    assert_eq!(kept.source(), "good.json");
    assert_eq!(kept.target_for("101").expect("101").target_qty, 10);
    let health = env.reader.health(T0 + H);
    assert_eq!(health.state, "degraded");
    assert!(
        health.detail.starts_with("rejected: content hash mismatch")
            && health.detail.ends_with("keeping good.json from 1.0h ago"),
        "{}",
        health.detail
    );
    assert_eq!(health.last_ok_ns, T0);

    // The same bad pointer is not re-reported and does not unblock anything.
    assert_eq!(env.refresh(T0 + 2 * H), RefreshOutcome::Unchanged);
    assert!(!env.reader.increase_exposure_allowed());

    let next = edited(|f| {
        f.header = Some(header(T0 + 2 * H));
        f.valid_until_ns = T0 + 30 * H;
        f.positions[0].target_qty = 20;
    });
    env.publish("next.json", &next);
    let t = env.expect_loaded(T0 + 2 * H);
    assert_eq!(t.target_for("101").expect("101").target_qty, 20);
    assert!(env.reader.increase_exposure_allowed());
    assert!(env.reader.rejection().is_none());
    assert_eq!(env.reader.health(T0 + 2 * H).state, "ok");
}

#[test]
fn staleness_over_time_blocks_increases_without_any_new_file() {
    let mut env = Env::new();
    env.publish("t.json", &edited(|f| f.valid_until_ns = T0 + 100 * H));
    env.expect_loaded(T0);
    assert!(env.reader.increase_exposure_allowed());

    assert_eq!(env.refresh(T0 + 36 * H), RefreshOutcome::Unchanged);
    assert!(
        env.reader.increase_exposure_allowed(),
        "exactly max age is still fine"
    );

    assert_eq!(env.refresh(T0 + 37 * H), RefreshOutcome::Unchanged);
    assert!(!env.reader.increase_exposure_allowed());
    assert!(matches!(
        env.reader.stale_reason(),
        Some(RejectReason::TooOld { .. })
    ));
    assert!(env.reader.current().is_some(), "stale targets are still served");
    let health = env.reader.health(T0 + 37 * H);
    assert_eq!(health.state, "degraded");
    assert_eq!(
        health.detail,
        "stale: too old: produced 37.0h ago, max age 36.0h; keeping t.json from 37.0h ago"
    );
    assert_eq!(health.last_ok_ns, T0 + 36 * H);

    // Recovery needs a fresh file, not just time passing.
    env.publish("t2.json", &sample(T0 + 37 * H));
    env.expect_loaded(T0 + 37 * H);
    assert!(env.reader.increase_exposure_allowed());
}

#[test]
fn expiry_of_last_good_blocks_increases() {
    let mut env = Env::new();
    env.publish("t.json", &sample(T0)); // valid for 24 h
    env.expect_loaded(T0);
    assert_eq!(env.refresh(T0 + 24 * H), RefreshOutcome::Unchanged);
    assert!(matches!(
        env.reader.stale_reason(),
        Some(RejectReason::Expired { .. })
    ));
    assert!(!env.reader.increase_exposure_allowed());
}

#[test]
fn unchanged_pointer_is_not_reread_but_rewritten_file_is() {
    let mut env = Env::new();
    let tf = sample(T0);
    env.publish("t.json", &tf);
    env.expect_loaded(T0);
    assert_eq!(env.refresh(T0 + 1), RefreshOutcome::Unchanged);
    assert_eq!(env.refresh(T0 + 2), RefreshOutcome::Unchanged);

    // Rewriting the pointer with identical bytes is still "unchanged".
    env.point_to("t.json", &tf.content_sha256);
    assert_eq!(env.refresh(T0 + 3), RefreshOutcome::Unchanged);

    // Rewriting the named file in place (same pointer) is noticed and verified again.
    let mut tampered = tf.clone();
    tampered.notes = "rewritten in place with a longer note".into();
    env.write_file("t.json", &tampered);
    assert!(matches!(
        env.expect_rejected(T0 + 4),
        RejectReason::ContentHashMismatch { .. }
    ));
}

#[test]
fn repeated_pointer_failure_is_reported_once() {
    let mut env = Env::new();
    env.publish("t.json", &sample(T0));
    env.expect_loaded(T0);
    fs::remove_file(env.path("current.json")).expect("remove pointer");
    assert!(matches!(
        env.expect_rejected(T0 + 1),
        RejectReason::PointerMissing { .. }
    ));
    assert_eq!(env.refresh(T0 + 2), RefreshOutcome::Unchanged);
    assert!(!env.reader.increase_exposure_allowed());

    // The same pointer coming back is a fresh, valid load.
    env.point_to("t.json", &sample(T0).content_sha256);
    env.expect_loaded(T0 + 3);
    assert!(env.reader.increase_exposure_allowed());
}

#[test]
fn config_from_core_uses_shared_toml() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = fs::read_to_string(root.join("config/susq.toml")).expect("shared config");
    let cfg = susq_core::Config::from_toml_str(&text).expect("shared config parses");
    let rc = ReaderConfig::from_core(&cfg, &root);
    assert_eq!(rc.dir, root.join("targets"));
    assert_eq!(rc.max_age, Duration::from_secs(36 * 3_600));
    assert_eq!(rc.tournament_slug, SLUG);
    assert_eq!(rc.future_skew, ReaderConfig::DEFAULT_FUTURE_SKEW);
}
