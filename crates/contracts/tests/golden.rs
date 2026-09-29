//! Cross-language golden round-trip: the Python reference wrote every fixture; Rust must decode
//! both the JSON and the binary form and re-encode to exactly the same bytes.

use std::{collections::BTreeMap, fs, path::PathBuf};

use prost::Message;
use susq_contracts::{decode, from_json, target_content_sha256, v1};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures")
}

fn roundtrip<M>(name: &str)
where
    M: Message + Default + PartialEq + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned,
{
    let bin = fs::read(dir().join(format!("{name}.binpb"))).expect("binpb");
    let json = fs::read_to_string(dir().join(format!("{name}.json"))).expect("json");

    let from_bin: M = decode(&bin).expect("decode binary");
    let from_js: M = from_json(&json).expect("decode json");
    assert_eq!(
        from_bin, from_js,
        "{name}: JSON and binary disagree field-for-field"
    );
    assert_eq!(
        from_bin.encode_to_vec(),
        bin,
        "{name}: binary re-encode differs from Python bytes"
    );

    // Our own JSON must parse back to the same message.
    let ours = susq_contracts::to_json(&from_bin).expect("to_json");
    let back: M = from_json(&ours).expect("reparse");
    assert_eq!(back, from_bin, "{name}: Rust JSON does not round-trip");
}

#[test]
fn every_fixture_round_trips() {
    let manifest: BTreeMap<String, String> =
        serde_json::from_str(&fs::read_to_string(dir().join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    let mut seen = 0;
    for (name, ty) in &manifest {
        match ty.as_str() {
            "susq.v1.TargetFile" => roundtrip::<v1::TargetFile>(name),
            "susq.v1.TargetPointer" => roundtrip::<v1::TargetPointer>(name),
            "susq.v1.CountyResultsBatch" => roundtrip::<v1::CountyResultsBatch>(name),
            "susq.v1.RacePosteriors" => roundtrip::<v1::RacePosteriors>(name),
            "susq.v1.NationalShift" => roundtrip::<v1::NationalShift>(name),
            "susq.v1.Heartbeat" => roundtrip::<v1::Heartbeat>(name),
            "susq.v1.Kill" => roundtrip::<v1::Kill>(name),
            "susq.v1.JournalEntry" => roundtrip::<v1::JournalEntry>(name),
            "susq.v1.OrderIntent" => roundtrip::<v1::OrderIntent>(name),
            "susq.v1.TraderLease" => roundtrip::<v1::TraderLease>(name),
            "susq.v1.StatusSnapshot" => roundtrip::<v1::StatusSnapshot>(name),
            "susq.v1.MarketMap" => roundtrip::<v1::MarketMap>(name),
            "susq.v1.Heartbeat@unknown" => continue,
            other => panic!("fixture {name} has unhandled type {other}; add it to this test"),
        }
        seen += 1;
    }
    assert!(seen >= 14, "expected at least 14 typed fixtures, saw {seen}");
}

#[test]
fn unknown_future_fields_are_tolerated() {
    let bin = fs::read(dir().join("heartbeat_unknown_field.binpb")).expect("fixture");
    let known = fs::read(dir().join("heartbeat.binpb")).expect("fixture");
    let hb: v1::Heartbeat = decode(&bin).expect("must decode despite field 99");
    assert_eq!(hb, decode::<v1::Heartbeat>(&known).expect("known"));
}

#[test]
fn target_hash_matches_python() {
    let tf: v1::TargetFile =
        from_json(&fs::read_to_string(dir().join("target_file.json")).expect("json")).expect("parse");
    assert_eq!(target_content_sha256(&tf), tf.content_sha256);
    let mut tampered = tf.clone();
    tampered.positions[0].target_qty += 1;
    assert_ne!(target_content_sha256(&tampered), tf.content_sha256);
}

#[test]
fn schema_version_is_enforced() {
    let mut h = v1::Header {
        schema_version: susq_contracts::SCHEMA_VERSION,
        ..Default::default()
    };
    assert!(susq_contracts::check_header(Some(&h)).is_ok());
    h.schema_version = 99;
    assert!(susq_contracts::check_header(Some(&h)).is_err());
    assert!(susq_contracts::check_header(None).is_err());
}
