//! Generated `susq.v1` contract types plus the encoding rules every Rust producer/consumer shares.
//!
//! * Binary: `prost` deterministic encoding (field-number order; we use no maps).
//! * Files: canonical proto3 JSON via `pbjson` (lowerCamelCase, int64 as strings).
//! * Target files: `content_sha256` = sha256 of the binary encoding with that field cleared.
//!
//! Golden fixtures in `/contracts/fixtures` (written by the Python reference) must round-trip
//! byte-for-byte; see `tests/golden.rs`.

#[allow(clippy::all, clippy::pedantic, clippy::unwrap_used, missing_docs)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/susq.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/susq.v1.serde.rs"));
}

use prost::Message;
use sha2::{Digest, Sha256};

/// Schema version this build reads and writes.
pub const SCHEMA_VERSION: u32 = v1::SchemaVersion::Current as u32;

#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protobuf decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("schema version {got} not supported (want {want})")]
    SchemaVersion { got: u32, want: u32 },
    #[error("missing header")]
    MissingHeader,
}

/// Deterministic binary encoding.
pub fn encode<M: Message>(msg: &M) -> Vec<u8> {
    msg.encode_to_vec()
}

/// Decode binary protobuf. Unknown fields are skipped (forward compatible).
pub fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, ContractError> {
    Ok(M::decode(bytes)?)
}

/// Canonical proto3 JSON (pretty).
pub fn to_json<M: serde::Serialize>(msg: &M) -> Result<String, ContractError> {
    Ok(serde_json::to_string_pretty(msg)? + "\n")
}

/// Parse proto3 JSON (accepts camelCase and original field names).
pub fn from_json<M: serde::de::DeserializeOwned>(text: &str) -> Result<M, ContractError> {
    Ok(serde_json::from_str(text)?)
}

/// `content_sha256` for a target file: sha256 over the encoding with the hash field cleared.
pub fn target_content_sha256(tf: &v1::TargetFile) -> String {
    let mut clone = tf.clone();
    clone.content_sha256.clear();
    hex::encode(Sha256::digest(clone.encode_to_vec()))
}

/// Reject headers from an incompatible schema.
pub fn check_header(h: Option<&v1::Header>) -> Result<&v1::Header, ContractError> {
    let h = h.ok_or(ContractError::MissingHeader)?;
    if h.schema_version != SCHEMA_VERSION {
        return Err(ContractError::SchemaVersion {
            got: h.schema_version,
            want: SCHEMA_VERSION,
        });
    }
    Ok(h)
}
