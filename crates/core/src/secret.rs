//! A string that never prints itself (API keys, realtime tokens).

use std::fmt;

/// Secret text. `Debug`/`Display` are redacted; use [`Secret::expose`] only where the value
/// must go on the wire (the `Authorization` header).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The raw value. Call sites should be few and obvious.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// True when empty (e.g. no key configured).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_prints() {
        let s = Secret::new("abc123");
        assert!(!format!("{s:?} {s}").contains("abc123"));
        assert_eq!(s.expose(), "abc123");
    }
}
