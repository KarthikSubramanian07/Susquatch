//! Paper vs live. Paper is the default; live needs BOTH `SUSQ_MODE=live` in the environment AND
//! `ops/GO_LIVE` containing "go live" (written by a human).

use std::path::Path;

use susq_core::Mode;

/// True when the go-live file exists and contains the phrase "go live" (case-insensitive).
pub fn go_live_armed(go_live_file: &Path) -> bool {
    std::fs::read_to_string(go_live_file).is_ok_and(|t| t.to_ascii_lowercase().contains("go live"))
}

/// Resolve the mode from the environment value of `SUSQ_MODE` and the go-live file.
pub fn resolve_mode(env_mode: Option<&str>, go_live_file: &Path) -> Mode {
    let env_live = env_mode.is_some_and(|m| m.trim().eq_ignore_ascii_case("live"));
    if env_live && go_live_armed(go_live_file) {
        Mode::Live
    } else {
        Mode::Paper
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_needs_both_env_and_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let f = dir.path().join("GO_LIVE");
        assert_eq!(resolve_mode(Some("live"), &f), Mode::Paper, "no file");
        std::fs::write(&f, "i think so").expect("write");
        assert_eq!(resolve_mode(Some("live"), &f), Mode::Paper, "wrong phrase");
        std::fs::write(&f, "Go Live\n").expect("write");
        assert_eq!(resolve_mode(Some("live"), &f), Mode::Live);
        assert_eq!(resolve_mode(Some("paper"), &f), Mode::Paper, "env not live");
        assert_eq!(resolve_mode(None, &f), Mode::Paper);
    }
}
