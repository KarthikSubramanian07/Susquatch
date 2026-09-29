"""Container healthchecks for processes that don't serve `/healthz` (run by Docker, exit 0 = healthy).

    python -m susq.ops.healthcheck bridge

The bridge (`susq-bridge open`) first waits for `SUSQ_API_KEY` to appear in `.env`, then for
`calendar.open_utc`, then runs `measure_day1`, and only then starts writing `state/status.json` every 60 s.
Waiting is healthy. After that the process is alive if it wrote either its status file or its journal
(every platform request/response, including `measure_day1`'s probes) within `MAX_SILENCE_S`. A bridge
that is silent past that is hung, and the watchdog restarts it. It reconciles from REST before acting
again, and its restart budget stops a crash loop.
"""

from __future__ import annotations

import os
import sys
from collections.abc import Mapping
from datetime import datetime, timedelta
from pathlib import Path

from dotenv import dotenv_values

from susq.ops.common import load_config, parse_utc, susq_home, utc_now

MAX_SILENCE_S = 600


def _api_key(home: Path, env: Mapping[str, str]) -> str:
    """Same precedence as the bridge: the environment over `.env`."""
    merged = (
        {k: v for k, v in dotenv_values(home / ".env").items() if v is not None}
        if (home / ".env").exists()
        else {}
    )
    merged.update(env)
    return merged.get("SUSQ_API_KEY", "") or ""


def _mtime(path: Path) -> float | None:
    try:
        return path.stat().st_mtime
    except OSError:
        return None


def bridge(home: Path, env: Mapping[str, str], now: datetime) -> tuple[bool, str]:
    """(healthy, why) for the Oct-1 bridge."""
    if not _api_key(home, env):
        return True, "waiting for SUSQ_API_KEY"
    config = load_config(home)
    open_text = config.get("calendar", {}).get("open_utc")
    if open_text and now < parse_utc(open_text):
        return True, "waiting for the open"
    candidates = [home / "state" / "status.json"]
    for day in (now.date(), (now - timedelta(days=1)).date()):
        candidates.append(home / "journal" / day.isoformat() / "bridge.journal.jsonl")
    times = [t for t in (_mtime(p) for p in candidates) if t is not None]
    if not times:
        return False, "no status.json or journal written since the open"
    silence = now.timestamp() - max(times)
    if silence > MAX_SILENCE_S:
        return False, f"no status or journal activity for {int(silence)} s"
    return True, f"active {int(silence)} s ago"


CHECKS = {"bridge": bridge}


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 1 or args[0] not in CHECKS:
        print(f"usage: python -m susq.ops.healthcheck {{{','.join(CHECKS)}}}", file=sys.stderr)
        return 2
    ok, why = CHECKS[args[0]](susq_home(), dict(os.environ), utc_now())
    print(why)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
