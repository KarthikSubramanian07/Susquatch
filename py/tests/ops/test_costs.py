from __future__ import annotations

import json
import subprocess

import httpx
import pytest
import respx
from susq.ops import costs
from susq.ops.common import load_config
from susq.ops.costs import (
    backup_remote,
    backup_row,
    bucket_bytes_live,
    cloudflare_rows,
    design_budget,
    fetch_cf_paid_subscriptions,
    github_rows,
    host_row,
    render,
)

from .conftest import T0


def test_host_b2pls_v2_shape_is_ok() -> None:
    # What the kernel reports on Standard_B2pls_v2 with the 30 GB disk: a little under 4 GB.
    row = host_row(system="Linux", machine="aarch64", cpus=2, mem_gb=3.8, disk_gb=29)
    assert row.status == "ok"
    assert "Azure for Students" in row.service
    assert "no card" in row.service


@pytest.mark.parametrize(
    ("cpus", "mem_gb", "disk_gb"),
    [(4, 3.8, 29), (2, 8.0, 29), (2, 3.8, 128)],
    ids=["more-vcpus", "more-ram", "bigger-disk"],
)
def test_host_bigger_than_b2pls_v2_fails(cpus: int, mem_gb: float, disk_gb: float) -> None:
    row = host_row(system="Linux", machine="aarch64", cpus=cpus, mem_gb=mem_gb, disk_gb=disk_gb)
    assert row.status == "fail"
    assert "credit" in row.note


def test_host_unmeasured_off_server() -> None:
    assert (
        host_row(system="Darwin", machine="arm64", cpus=10, mem_gb=None, disk_gb=None).status == "unmeasured"
    )


def test_github_net_charges_fail() -> None:
    summary = {
        "usageItems": [
            {"product": "Actions", "unitType": "Minutes", "grossQuantity": 300, "netAmount": 0},
            {"product": "Packages", "unitType": "GigabyteHours", "grossQuantity": 5, "netAmount": 0.25},
        ]
    }
    (row,) = github_rows(summary)
    assert row.status == "fail"
    assert "300 min" in row.usage


def test_github_minutes_headroom() -> None:
    summary = {
        "usageItems": [{"product": "actions", "unitType": "minutes", "grossQuantity": 1700, "netAmount": 0}]
    }
    assert github_rows(summary)[0].status == "warn"
    assert github_rows(None)[0].status == "unmeasured"


def test_design_budget_from_repo_config_fits_free_tiers(home) -> None:
    d = design_budget(load_config(home))
    assert d.kv_writes < costs.CF_KV_WRITES_PER_DAY
    assert d.requests < costs.CF_REQUESTS_PER_DAY
    rows = cloudflare_rows(d, kv_writes_today=120, paid_subscriptions=[])
    assert all(r.status in {"ok", "unmeasured"} for r in rows), rows


def test_config_that_would_blow_kv_writes_fails() -> None:
    d = design_budget({"ops": {"status_push_interval_s": 30, "status_push_daily_budget": 5000}})
    rows = cloudflare_rows(d, kv_writes_today=None, paid_subscriptions=None)
    kv = next(r for r in rows if r.service == "Workers KV writes")
    assert kv.status == "fail"


def test_backup_row_tracks_the_b2_free_tier() -> None:
    assert backup_row(None, "").status == "unmeasured"
    assert backup_row(500_000_000, "rclone size").status == "ok"
    near = backup_row(8_500_000_000, "rclone size")
    assert near.status == "warn"
    assert "Backblaze B2" in near.service
    assert "85%" in near.usage
    assert backup_row(10_000_000_000, "x").status == "fail"


def test_backup_remote_backblaze_b2() -> None:
    env = {
        "SUSQ_R2_ACCESS_KEY_ID": "id",
        "SUSQ_R2_SECRET_ACCESS_KEY": "secret",
        "SUSQ_R2_ENDPOINT": "https://s3.us-west-004.backblazeb2.com",
        "SUSQ_R2_PROVIDER": "Other",
        "SUSQ_R2_BUCKET": "susq-b2",
    }
    live = backup_remote(env)  # no CLOUDFLARE_ACCOUNT_ID needed once an endpoint is given
    assert live is not None
    remote, rclone_env = live
    assert remote == "susqr2:susq-b2"
    assert rclone_env["RCLONE_CONFIG_SUSQR2_PROVIDER"] == "Other"
    assert rclone_env["RCLONE_CONFIG_SUSQR2_ENDPOINT"] == "https://s3.us-west-004.backblazeb2.com"


def test_backup_remote_defaults_and_gaps() -> None:
    creds = {"SUSQ_R2_ACCESS_KEY_ID": "id", "SUSQ_R2_SECRET_ACCESS_KEY": "secret"}
    assert backup_remote({}) is None
    assert backup_remote(creds) is None  # neither an endpoint nor a Cloudflare account to derive one
    live = backup_remote({**creds, "CLOUDFLARE_ACCOUNT_ID": "acc"})
    assert live is not None
    remote, rclone_env = live
    assert remote == "susqr2:susquatch-backups"
    assert rclone_env["RCLONE_CONFIG_SUSQR2_PROVIDER"] == "Cloudflare"
    assert rclone_env["RCLONE_CONFIG_SUSQR2_ENDPOINT"] == "https://acc.r2.cloudflarestorage.com"
    assert backup_remote({"SUSQ_BACKUP_REMOTE": "/tmp/drill"}) == ("/tmp/drill", {})


def test_paid_cloudflare_subscription_fails() -> None:
    with respx.mock() as api:
        api.get("https://api.cloudflare.com/client/v4/accounts/acc/subscriptions").respond(
            200,
            json={
                "result": [
                    {"id": "1", "price": 0, "rate_plan": {"id": "free"}},
                    {"id": "2", "price": 5, "rate_plan": {"public_name": "Workers Paid"}},
                ]
            },
        )
        with httpx.Client() as client:
            paid = fetch_cf_paid_subscriptions(client, "acc", "tok")
    assert paid == ["Workers Paid"]
    rows = cloudflare_rows(design_budget({}), kv_writes_today=None, paid_subscriptions=paid)
    assert next(r for r in rows if r.service == "Cloudflare billing").status == "fail"


def test_bucket_bytes_live_parses_rclone(monkeypatch) -> None:
    monkeypatch.setattr(costs.shutil, "which", lambda _n: "/usr/bin/rclone")

    def runner(argv, **_kw):
        return subprocess.CompletedProcess(argv, 0, stdout=json.dumps({"count": 3, "bytes": 1234}), stderr="")

    assert bucket_bytes_live("susqr2:b", runner) == 1234

    def failing(argv, **_kw):
        raise subprocess.CalledProcessError(1, argv)

    assert bucket_bytes_live("susqr2:b", failing) is None


def test_audit_without_credentials_passes_and_prints_human_checklist(home, monkeypatch) -> None:
    monkeypatch.setattr(costs, "_gh_token", lambda _env: "")
    with respx.mock(assert_all_called=False), httpx.Client() as client:
        rows = costs.audit(home, {}, client, now=T0)
    text = render(rows)
    assert "RESULT: $0/month" in text
    assert "Azure for Students (no card, credit-funded); no Pay-As-You-Go upgrade" in text
    assert "Backblaze B2" in text
    assert not any(r.status == "fail" for r in rows)


def test_main_exit_code_reflects_failures(monkeypatch, capsys) -> None:
    monkeypatch.setattr(costs, "audit", lambda *a, **k: [costs.Row("x", "free", "paid", "fail")])
    assert costs.main([]) == 1
    assert "FAIL" in capsys.readouterr().out
    monkeypatch.setattr(costs, "audit", lambda *a, **k: [costs.Row("x", "free", "0", "ok")])
    assert costs.main(["--json"]) == 0


@pytest.mark.parametrize(("frac", "status"), [(0.1, "ok"), (0.85, "warn"), (1.0, "fail")])
def test_frac_status(frac: float, status: str) -> None:
    assert costs._frac_status(frac) == status
