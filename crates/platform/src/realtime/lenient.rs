//! Serde helpers that accept a JSON number or a numeric string for the same field.
//!
//! Realtime payloads are produced by a different code path than REST, so ids and revisions may
//! arrive as `381` or `"381"`. `null` decodes as the field's empty value; anything else that is
//! not a number (or a string that does not parse) is an error.

use serde::{Deserialize, Deserializer, de::Error};
use serde_json::Value;

/// Text of a string or number; `None` for `null`.
fn text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        Value::Number(n) => Ok(Some(n.to_string())),
        other => Err(D::Error::custom(format!(
            "expected a string or number, got {other}"
        ))),
    }
}

/// Parse a string or number into `T` via `FromStr`; `None` for `null`.
fn parsed<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    text(d)?
        .map(|s| s.trim().parse::<T>().map_err(D::Error::custom))
        .transpose()
}

/// A required id (`ExchangeId`, `MarketId`, ...) from a string or number.
pub(crate) fn id<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: From<String>,
{
    text(d)?
        .map(T::from)
        .ok_or_else(|| D::Error::custom("id must not be null"))
}

/// An optional id from a string, number or `null`.
pub(crate) fn opt_id<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: From<String>,
{
    Ok(text(d)?.map(T::from))
}

/// Optional free text from a string, number or `null`.
pub(crate) fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    text(d)
}

/// An optional unsigned integer (revisions) from a number, numeric string or `null`.
pub(crate) fn opt_u64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    parsed(d)
}

/// An optional signed integer (order ids) from a number, numeric string or `null`.
pub(crate) fn opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    parsed(d)
}

/// A float from a number or numeric string; `null` is `0.0`.
pub(crate) fn f64<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(parsed(d)?.unwrap_or_default())
}

/// An optional float from a number, numeric string or `null`.
pub(crate) fn opt_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    parsed(d)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    struct Probe {
        #[serde(deserialize_with = "super::id")]
        id: String,
        #[serde(default, deserialize_with = "super::opt_u64")]
        rev: Option<u64>,
        #[serde(default, deserialize_with = "super::f64")]
        price: f64,
    }

    fn probe(v: serde_json::Value) -> Result<Probe, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn numbers_and_numeric_strings_are_equivalent() {
        let a = probe(json!({"id": 381, "rev": 7, "price": 0.45})).expect("numbers");
        let b = probe(json!({"id": "381", "rev": "7", "price": "0.45"})).expect("strings");
        assert_eq!((a.id.as_str(), a.rev, a.price), ("381", Some(7), 0.45));
        assert_eq!((b.id.as_str(), b.rev, b.price), ("381", Some(7), 0.45));
    }

    #[test]
    fn null_and_missing_are_empty() {
        let p = probe(json!({"id": "x", "rev": null, "price": null})).expect("nulls");
        assert_eq!((p.rev, p.price), (None, 0.0));
        let p = probe(json!({"id": "x"})).expect("missing");
        assert_eq!((p.rev, p.price), (None, 0.0));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(probe(json!({"id": null})).is_err());
        assert!(probe(json!({"id": "x", "rev": "seven"})).is_err());
        assert!(probe(json!({"id": "x", "rev": -1})).is_err());
        assert!(probe(json!({"id": ["x"]})).is_err());
    }
}
