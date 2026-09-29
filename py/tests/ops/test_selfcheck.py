from __future__ import annotations

import json
import os
from datetime import date, timedelta

import httpx
import pytest
import respx
from susq.ops import selfcheck
from susq.ops.common import ActionItems
from susq.ops.selfcheck import (
    Check,
    check_backup,
    check_clock,
    check_disk,
    check_expiries,
    check_lease,
    check_quotas,
    check_tls,
    measure_drift,
    record,
    run_checks,
)

from .conftest import T0


def test_clock_thresholds() -> None:
    assert check_clock(0.4).state == "ok"
    assert check_clock(-2.5).state == "degraded"
    down = check_clock(7.0)
    assert down.state == "down"
    assert "chronyc" in (down.action or "")
    assert check_clock(None).state == "degraded"


def test_measure_drift_uses_http_date_median() -> None:
    with respx.mock(assert_all_called=False) as api:
        api.head("https://a.test").respond(200, headers={"date": "Mon, 05 Oct 2026 12:00:10 GMT"})
        api.head("https://b.test").respond(200, headers={"date": "Mon, 05 Oct 2026 12:00:10 GMT"})
        api.head("https://c.test").mock(side_effect=httpx.ConnectError("x"))
        with httpx.Client() as client:
            drift = measure_drift(client, ["https://a.test", "https://b.test", "https://c.test"])
    # The machine's real clock is nowhere near the fake Date, so the value is huge and finite.
    assert drift is not None
    assert abs(drift) > 1000


def test_measure_drift_none_when_nothing_answers() -> None:
    with respx.mock() as api:
        api.head("https://a.test").mock(side_effect=httpx.ConnectError("x"))
        with httpx.Client() as client:
            assert measure_drift(client, ["https://a.test"]) is None


def test_expiries() -> None:
    today = date(2026, 10, 5)
    assert check_expiries({}, today=today).state == "ok"
    ok = check_expiries({"SUSQ_EXPIRES_GITHUB_TOKEN": "2027-01-01"}, today=today)
    assert ok.state == "ok"
    assert ok.action is None
    soon = check_expiries({"SUSQ_EXPIRES_GITHUB_TOKEN": "2026-10-12"}, today=today)
    assert soon.state == "degraded"
    assert "github_token in 7d" in soon.detail
    gone = check_expiries({"SUSQ_EXPIRES_SUPERMARKET_KEY": "2026-10-01", "OTHER": "x"}, today=today)
    assert gone.state == "down"
    assert "supermarket_key" in (gone.action or "")
    bad = check_expiries({"SUSQ_EXPIRES_X": "next tuesday"}, today=today)
    assert bad.state == "degraded"


def test_tls() -> None:
    assert check_tls(["a", "b"], {"a": 80.0, "b": 40.0}.__getitem__).detail == "soonest b in 40d"
    assert check_tls(["a"], lambda _h: 3.0).state == "degraded"

    def boom(_h: str) -> float:
        raise OSError("unreachable")

    assert check_tls(["a"], boom).state == "degraded"


def test_lease(paths) -> None:
    open_utc = T0 - timedelta(days=1)
    assert check_lease(paths.lease_file, now=T0 - timedelta(days=2), open_utc=open_utc).state == "ok"
    missing = check_lease(paths.lease_file, now=T0, open_utc=open_utc)
    assert missing.state == "down"
    assert missing.action

    exp = int((T0 + timedelta(seconds=20)).timestamp() * 1e9)
    paths.lease_file.write_text(json.dumps({"holder": "bridge", "expiresAtNs": str(exp)}))
    assert check_lease(paths.lease_file, now=T0, open_utc=open_utc).detail == "held by bridge"
    late = check_lease(paths.lease_file, now=T0 + timedelta(minutes=10), open_utc=open_utc)
    assert late.state == "down"
    assert "bridge" in late.detail


def test_backup_freshness() -> None:
    assert check_backup(None, now=T0, max_age_h=30).action
    fresh = {"last_success_utc": (T0 - timedelta(hours=5)).strftime("%Y-%m-%dT%H:%M:%SZ")}
    assert check_backup(fresh, now=T0, max_age_h=30).state == "ok"
    stale = {"last_success_utc": (T0 - timedelta(hours=50)).strftime("%Y-%m-%dT%H:%M:%SZ")}
    assert check_backup(stale, now=T0, max_age_h=30).state == "down"


@pytest.mark.parametrize(
    ("used", "state"),
    [(1_000_000_000, "ok"), (7_000_000_000, "degraded"), (8_000_000_000, "down")],
)
def test_r2_quota_guard(used: int, state: str) -> None:
    c = check_quotas(
        {"bucket_bytes": used},
        {"date": "2026-10-05", "count": 300},
        r2_quota=10_000_000_000,
        guard_frac=0.8,
        push_budget=800,
        today="2026-10-05",
    )
    assert c.state == state
    assert "KV pushes 300/800" in c.detail
    assert (c.action is not None) == (state == "down")


def test_disk_self_heals_stale_staging(home, paths) -> None:
    staging = paths.ops_state / "backup-staging"
    staging.mkdir(parents=True)
    old = staging / "susq-old.tar.zst"
    old.write_bytes(b"x" * 2_000_000)
    os.utime(old, (T0.timestamp() - 3 * 86400, T0.timestamp() - 3 * 86400))
    fresh = staging / "susq-new.tar.zst"
    fresh.write_bytes(b"x")
    os.utime(fresh, (T0.timestamp(), T0.timestamp()))
    c = check_disk(home, staging, now=T0)
    assert not old.exists()
    assert fresh.exists()
    assert "removed 2 MB" in c.detail


def test_record_syncs_action_items(paths) -> None:
    actions = ActionItems(paths.actions_dir)
    actions.raise_item("selfcheck-clock", "old clock item")
    actions.raise_item("r2-quota", "owned by backup.sh")
    out = record(
        paths,
        [Check("clock", "ok", "fine"), Check("backup", "down", "stale", "Backups are stale")],
        now=T0,
    )
    assert out["state"] == "down"
    assert out["summary"] == "1/2 ok; attention: backup"
    assert sorted(actions.items()) == ["Backups are stale", "owned by backup.sh"]
    assert json.loads(paths.selfcheck_record.read_text())["checks"][1]["name"] == "backup"


def test_run_checks_end_to_end_offline(paths, home) -> None:
    from susq.ops.common import load_config

    checks = run_checks(
        paths,
        load_config(home),
        {"SUSQ_EXPIRES_GITHUB_TOKEN": "2027-06-01"},
        now=T0,
        drift_s=0.3,
        tls_hosts=["www.thesuper.market"],
        tls_days=lambda _h: 60.0,
    )
    by_name = {c.name: c for c in checks}
    assert set(by_name) == {"disk", "clock", "expiry", "tls", "lease", "backup", "quotas"}
    assert by_name["lease"].state == "down"  # T0 is after open and no lease exists
    assert selfcheck.SEVERITY[by_name["clock"].state] == 0
