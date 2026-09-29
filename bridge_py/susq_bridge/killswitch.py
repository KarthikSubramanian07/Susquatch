"""Kill switch (``ops/KILL``) and live-mode arming (``ops/GO_LIVE`` + ``SUSQ_MODE=live``).

Presence of the kill file means cancel-all and freeze; its text is the reason. Live trading needs two
independent switches: the environment must say ``live`` and a human must have written "go live" into
the go-live file. Anything else, including an unreadable file, resolves to paper.
"""

from __future__ import annotations

from collections.abc import Mapping
from pathlib import Path
from typing import Literal

from susq_bridge.lease import atomic_write

__all__ = ["KillSwitch", "resolve_mode"]

_DEFAULT_REASON = "killed"
_GO_LIVE_PHRASE = "go live"


class KillSwitch:
    """Reads the kill and go-live files fresh on every call; nothing is cached."""

    def __init__(self, kill_file: Path, go_live_file: Path) -> None:
        self._kill_file = kill_file
        self._go_live_file = go_live_file

    def reason(self) -> str | None:
        """None if not killed; otherwise the kill file's text, or "killed" if it is empty/unreadable."""
        try:
            text = self._kill_file.read_text()
        except FileNotFoundError:
            return None
        except (OSError, UnicodeDecodeError):
            return _DEFAULT_REASON
        return text.strip() or _DEFAULT_REASON

    def killed(self) -> bool:
        return self.reason() is not None

    def go_live_armed(self) -> bool:
        """True if the go-live file exists and says "go live" (any case, any whitespace between)."""
        try:
            text = self._go_live_file.read_text()
        except (OSError, UnicodeDecodeError):
            return False
        return _GO_LIVE_PHRASE in " ".join(text.split()).lower()

    def trigger(self, reason: str) -> None:
        """Write the kill file atomically with ``reason`` as its text."""
        atomic_write(self._kill_file, reason.encode())


def resolve_mode(default: str, env: Mapping[str, str], kill: KillSwitch) -> Literal["paper", "live"]:
    """``live`` only if ``SUSQ_MODE`` (else ``default``) is live AND the go-live file is armed."""
    wants_live = env.get("SUSQ_MODE", default).strip().lower() == "live"
    return "live" if wants_live and kill.go_live_armed() else "paper"
