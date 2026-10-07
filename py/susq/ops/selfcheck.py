"""Daily self-check: find what will break before it breaks, fix what is safe to fix, file the rest.

Checks: disk headroom, clock drift (vs HTTP `Date` from several independent hosts), secret expiry dates,
TLS certificate expiry of the hosts we depend on, trader-lease health, backup freshness, and free-tier
headroom (backup bucket bytes, KV writes). Results go to `ops/state/selfcheck.json`, where the status pusher
turns them into a health tile. Anything a human must do becomes a `selfcheck-*` ACTION NEEDED item, which
appears in the next daily summary. Items clear themselves once the check passes again.

Runs from a systemd timer (the Azure server), launchd (the Mac fallback), or `make selfcheck`. It never
alerts and never trades.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import ssl
import statistics
import time
from collections.abc import Callable, Iterable
from dataclasses import asdict, dataclass
from datetime import date, datetime
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

import httpx

from susq.ops.common import (
    ActionItems,
    OpsPaths,
    atomic_write_text,
    load_config,
    ops_setting,
    parse_utc,
    susq_home,
    utc_now,
)

SEVERITY = {"ok": 0, "degraded": 1, "down": 2}
TIME_HOSTS = ("https://www.cloudflare.com", "https://api.github.com", "https://www.google.com")
EXPIRY_PREFIX = "SUSQ_EXPIRES_"
EXPIRY_WARN_DAYS = 14
TLS_WARN_DAYS = 7
DRIFT_WARN_S = 2.0
DRIFT_DOWN_S = 5.0


@dataclass
class Check:
    """One check's verdict. `action` is set only when a human has to do something."""

    name: str
    state: str
    detail: str
    action: str | None = None


# ── individual checks (pure where possible; I/O injected) ────────────────────────


def check_disk(home: Path, staging: Path, *, now: datetime) -> Check:
    """Free space on the $SUSQ_HOME filesystem. Self-heal: drop stale backup staging archives first."""
    freed = 0
    if staging.exists():
        cutoff = now.timestamp() - 86400
        for f in staging.glob("*.tar.zst*"):
            if f.stat().st_mtime < cutoff:
                freed += f.stat().st_size
                f.unlink(missing_ok=True)
    usage = shutil.disk_usage(home)
    free_frac = usage.free / usage.total
    detail = f"{usage.free / 1e9:.1f} GB free ({free_frac:.0%})"
    if freed:
        detail += f"; removed {freed / 1e6:.0f} MB of stale backup staging"
    if free_frac < 0.05:
        return Check(
            "disk", "down", detail, f"Disk almost full on the server: {detail}. Prune data/ or journal/."
        )
    if free_frac < 0.15:
        return Check("disk", "degraded", detail, f"Disk getting full: {detail}.")
    return Check("disk", "ok", detail)


def measure_drift(client: httpx.Client, urls: Iterable[str] = TIME_HOSTS) -> float | None:
    """Median offset (server Date - local clock) in seconds across hosts, or None if none answered."""
    offsets = []
    for url in urls:
        try:
            t0 = time.time()
            resp = client.head(url)
            t1 = time.time()
            stamp = resp.headers.get("date")
            if not stamp:
                continue
            server = parsedate_to_datetime(stamp).timestamp()
            # Date has 1 s resolution and is truncated, so compare against the midpoint minus half a second.
            offsets.append(server + 0.5 - (t0 + t1) / 2)
        except (httpx.HTTPError, ValueError, TypeError):
            continue
    return statistics.median(offsets) if offsets else None


def check_clock(drift_s: float | None) -> Check:
    if drift_s is None:
        return Check("clock", "degraded", "could not measure (no HTTP Date answered)")
    detail = f"drift {drift_s:+.1f}s vs HTTP Date"
    if abs(drift_s) >= DRIFT_DOWN_S:
        return Check(
            "clock", "down", detail, f"Server clock is off by {drift_s:+.1f}s; check `chronyc tracking`."
        )
    if abs(drift_s) >= DRIFT_WARN_S:
        return Check("clock", "degraded", detail)
    return Check("clock", "ok", detail)


def check_expiries(env: dict[str, str], *, today: date) -> Check:
    """`SUSQ_EXPIRES_<NAME>=YYYY-MM-DD` in .env, one per credential that expires (API key, tokens)."""
    soon: list[str] = []
    expired: list[str] = []
    bad: list[str] = []
    tracked = 0
    for key, value in sorted(env.items()):
        if not key.startswith(EXPIRY_PREFIX) or not value.strip():
            continue
        tracked += 1
        name = key.removeprefix(EXPIRY_PREFIX).lower()
        try:
            expires = date.fromisoformat(value.strip())
        except ValueError:
            bad.append(name)
            continue
        days = (expires - today).days
        if days < 0:
            expired.append(f"{name} ({-days}d ago)")
        elif days <= EXPIRY_WARN_DAYS:
            soon.append(f"{name} in {days}d")
    if expired:
        return Check(
            "expiry", "down", "expired: " + ", ".join(expired), "Credential expired: " + ", ".join(expired)
        )
    if soon or bad:
        parts = [f"expiring {', '.join(soon)}"] if soon else []
        parts += [f"unparseable date for {', '.join(bad)}"] if bad else []
        detail = "; ".join(parts)
        return Check("expiry", "degraded", detail, f"Rotate soon: {detail}.")
    if tracked == 0:
        return Check("expiry", "ok", "no expiry dates recorded (set SUSQ_EXPIRES_* in .env)")
    return Check("expiry", "ok", f"{tracked} credential(s) valid > {EXPIRY_WARN_DAYS}d")


def tls_days_left(host: str, *, timeout: float = 5.0) -> float:
    """Days until the TLS certificate served by `host:443` expires."""
    ctx = ssl.create_default_context()
    with (
        socket.create_connection((host, 443), timeout=timeout) as sock,
        ctx.wrap_socket(sock, server_hostname=host) as tls,
    ):
        cert = tls.getpeercert()
    not_after = ssl.cert_time_to_seconds(str(cert["notAfter"]))  # type: ignore[index]
    return (not_after - time.time()) / 86400


def check_tls(hosts: Iterable[str], days_left: Callable[[str], float]) -> Check:
    """Upstream certs aren't ours to renew; a near-expiry is reported so a sudden TLS failure is explained."""
    worst: tuple[float, str] | None = None
    errors = []
    for host in hosts:
        try:
            d = days_left(host)
        except (OSError, ssl.SSLError, ValueError) as exc:
            errors.append(f"{host}: {type(exc).__name__}")
            continue
        if worst is None or d < worst[0]:
            worst = (d, host)
    if worst is None:
        return Check("tls", "degraded", "could not check: " + "; ".join(errors))
    detail = f"soonest {worst[1]} in {worst[0]:.0f}d"
    if errors:
        detail += "; unreachable: " + ", ".join(errors)
    return Check("tls", "degraded" if worst[0] < TLS_WARN_DAYS or errors else "ok", detail)


def check_lease(lease_file: Path, *, now: datetime, open_utc: datetime | None) -> Check:
    """Someone should hold an unexpired trader lease once trading is open."""
    if open_utc is not None and now < open_utc:
        return Check("lease", "ok", "trading not open yet")
    try:
        lease = json.loads(lease_file.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return Check(
            "lease",
            "down",
            "no trader lease: nothing is trading",
            "No process holds the trader lease, so nothing is trading. Check `make logs`.",
        )
    holder = str(lease.get("holder", "?"))
    expires_ns = int(lease.get("expiresAtNs", 0))
    late_s = now.timestamp() - expires_ns / 1e9
    if late_s > 60:
        mins = int(late_s // 60)
        return Check(
            "lease",
            "down",
            f"{holder} lease expired {mins}m ago",
            f"Trader lease held by {holder} expired {mins}m ago and nobody took over. Check `make logs`.",
        )
    return Check("lease", "ok", f"held by {holder}")


def check_backup(record: dict[str, Any] | None, *, now: datetime, max_age_h: float) -> Check:
    if not record or not record.get("last_success_utc"):
        return Check(
            "backup",
            "degraded",
            "no successful backup yet",
            "No backup has succeeded yet: run `make backup`.",
        )
    age_h = (now - parse_utc(str(record["last_success_utc"]))).total_seconds() / 3600
    detail = f"last good backup {age_h:.0f}h ago"
    if age_h > max_age_h:
        return Check(
            "backup", "down", detail, f"Backups are stale ({detail}); see `journalctl -u susq-backup`."
        )
    return Check("backup", "ok", detail)


def check_quotas(
    backup: dict[str, Any] | None,
    push: dict[str, Any] | None,
    *,
    r2_quota: int,
    guard_frac: float,
    push_budget: int,
    today: str,
) -> Check:
    """Free-tier headroom we can measure on the server: backup bucket bytes (last backup) and KV pushes."""
    notes = []
    state = "ok"
    action = None
    if backup and backup.get("bucket_bytes") is not None:
        frac = int(backup["bucket_bytes"]) / r2_quota
        notes.append(f"backups {frac:.0%} of B2 free tier")
        if frac >= guard_frac:
            state = "down"
            action = f"Backups at {frac:.0%} of the 10 GB B2 free tier: uploads are paused. Prune old ones."
        elif frac >= guard_frac * 0.85:
            state = "degraded"
    if push and push.get("date") == today:
        used = int(push.get("count", 0))
        notes.append(f"KV pushes {used}/{push_budget} today")
    return Check("quotas", state, "; ".join(notes) or "nothing measured yet", action)


# ── orchestration ───────────────────────────────────────────────────────────────


NEEDLE_PRIOR_FROM = date(2026, 10, 20)  # before this the 2026 prior may not be staged yet
NEEDLE_PRIOR_MIN_RACES = 60
NEEDLE_PRIOR_MAX_AGE_D = 9.0


def check_needle_prior(prior: Path, *, now: datetime) -> Check:
    """The staged 2026 needle prior (state/needle/prior.json): present, full and recent from Oct 20."""
    required = now.date() >= NEEDLE_PRIOR_FROM
    try:
        doc = json.loads(prior.read_text(encoding="utf-8"))
        races = len(doc.get("races") or [])
        age_d = (now.timestamp() - prior.stat().st_mtime) / 86_400
    except (OSError, ValueError, AttributeError):
        if required:
            return Check(
                "needle-prior", "degraded", "no 2026 prior staged",
                "Stage the needle prior: ops/needle-prior.sh (docs/RUNBOOK.md §2 pre-flight).",
            )  # fmt: skip
        return Check("needle-prior", "ok", "not staged yet (needed from Oct 20)")
    detail = f"{races} races, built {age_d:.1f} d ago"
    if races < NEEDLE_PRIOR_MIN_RACES:
        return Check(
            "needle-prior", "degraded", detail, f"The needle prior has only {races} races: rebuild it."
        )
    if required and age_d > NEEDLE_PRIOR_MAX_AGE_D:
        return Check(
            "needle-prior", "degraded", detail, "The needle prior is stale: run ops/needle-prior.sh."
        )
    return Check("needle-prior", "ok", detail)


def _read_json(path: Path) -> dict[str, Any] | None:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def run_checks(
    paths: OpsPaths,
    config: dict[str, Any],
    env: dict[str, str],
    *,
    now: datetime,
    drift_s: float | None,
    tls_hosts: Iterable[str],
    tls_days: Callable[[str], float],
) -> list[Check]:
    open_text = config.get("calendar", {}).get("open_utc")
    backup = _read_json(paths.backup_record)
    return [
        check_disk(paths.home, paths.ops_state / "backup-staging", now=now),
        check_clock(drift_s),
        check_expiries(env, today=now.date()),
        check_tls(tls_hosts, tls_days),
        check_lease(paths.lease_file, now=now, open_utc=parse_utc(open_text) if open_text else None),
        check_backup(backup, now=now, max_age_h=float(ops_setting(config, "backup_max_age_h", 30))),
        check_needle_prior(paths.home / "state" / "needle" / "prior.json", now=now),
        check_quotas(
            backup,
            _read_json(paths.push_record),
            r2_quota=int(ops_setting(config, "r2_quota_bytes", 10_000_000_000)),
            guard_frac=float(ops_setting(config, "r2_guard_frac", 0.8)),
            push_budget=int(ops_setting(config, "status_push_daily_budget", 800)),
            today=now.strftime("%Y-%m-%d"),
        ),
    ]


def record(paths: OpsPaths, checks: list[Check], *, now: datetime) -> dict[str, Any]:
    """Write `selfcheck.json` and sync `selfcheck-*` ACTION NEEDED items with the results."""
    worst = max(checks, key=lambda c: SEVERITY.get(c.state, 2))
    bad = [c.name for c in checks if c.state != "ok"]
    summary = f"{len(checks) - len(bad)}/{len(checks)} ok" + (f"; attention: {', '.join(bad)}" if bad else "")
    out = {
        "state": worst.state,
        "summary": summary,
        "checked_at_utc": now.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "checks": [asdict(c) for c in checks],
    }
    atomic_write_text(paths.selfcheck_record, json.dumps(out, indent=2) + "\n")

    actions = ActionItems(paths.actions_dir)
    open_keys = set()
    for c in checks:
        if c.action:
            key = f"selfcheck-{c.name}"
            actions.raise_item(key, c.action)
            open_keys.add(key)
    actions.resolve_prefix("selfcheck-", keep=open_keys)
    return out


def _hosts(config: dict[str, Any], env: dict[str, str]) -> list[str]:
    urls = [
        config.get("platform", {}).get("base_url", "https://www.thesuper.market/api/v1"),
        env.get("SUSQ_DASHBOARD_URL", "https://susquatch.pages.dev"),
    ]
    return [h for h in (urlparse(u).hostname for u in urls) if h]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Daily self-check (never alerts)")
    parser.add_argument("--strict", action="store_true", help="exit 1 if anything is not ok")
    args = parser.parse_args(argv)

    home = susq_home()
    config = load_config(home)
    env = dict(os.environ)
    now = utc_now()
    with httpx.Client(timeout=5.0, follow_redirects=False) as client:
        drift = measure_drift(client)
    checks = run_checks(
        OpsPaths(home),
        config,
        env,
        now=now,
        drift_s=drift,
        tls_hosts=_hosts(config, env),
        tls_days=tls_days_left,
    )
    out = record(OpsPaths(home), checks, now=now)
    for c in checks:
        print(f"{c.state:>8}  {c.name:<8} {c.detail}")
    print(f"selfcheck: {out['summary']}")
    return 1 if args.strict and out["state"] != "ok" else 0


if __name__ == "__main__":
    raise SystemExit(main())
