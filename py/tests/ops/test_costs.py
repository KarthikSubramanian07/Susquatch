from __future__ import annotations

import json
import subprocess

import httpx
import pytest
import respx
from susq.ops import costs
from susq.ops.common import load_config
from susq.ops.costs import (
    cloudflare_rows,
    design_budget,
    fetch_cf_paid_subscriptions,
    github_rows,
    oracle_row,
    r2_bytes_live,
    render,
)

from .conftest import T0


def test_oracle_shape_within_always_free() -> None:
    row = oracle_row(system="Linux", machine="aarch64", cpus=2, mem_gb=11.7, disk_gb=100)
    assert row.status == "ok"


def test_oracle_shape_over_always_free_fails() -> None:
    row = oracle_row(system="Linux", machine="aarch64", cpus=4, mem_gb=24, disk_gb=100)
    assert row.status == "fail"
    assert "billable" in row.note


def test_oracle_unmeasured_off_server() -> None:
    assert (
        oracle_row(system="Darwin", machine="arm64", cpus=10, mem_gb=None, disk_gb=None).status
        == "unmeasured"
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
    rows = cloudflare_rows(d, kv_writes_today=120, r2_bytes=None, r2_source="", paid_subscriptions=[])
    assert all(r.status in {"ok", "unmeasured"} for r in rows), rows


def test_config_that_would_blow_kv_writes_fails() -> None:
    d = design_budget({"ops": {"status_push_interval_s": 30, "status_push_daily_budget": 5000}})
    rows = cloudflare_rows(d, kv_writes_today=None, r2_bytes=None, r2_source="", paid_subscriptions=None)
    kv = next(r for r in rows if r.service == "Workers KV writes")
    assert kv.status == "fail"


def test_r2_rows() -> None:
    d = design_budget({})
    near = cloudflare_rows(
        d, kv_writes_today=None, r2_bytes=8_500_000_000, r2_source="x", paid_subscriptions=None
    )
    assert next(r for r in near if r.service == "Cloudflare R2").status == "warn"


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
    rows = cloudflare_rows(
        design_budget({}), kv_writes_today=None, r2_bytes=None, r2_source="", paid_subscriptions=paid
    )
    assert next(r for r in rows if r.service == "Cloudflare billing").status == "fail"


def test_r2_bytes_live_parses_rclone(monkeypatch) -> None:
    monkeypatch.setattr(costs.shutil, "which", lambda _n: "/usr/bin/rclone")

    def runner(argv, **_kw):
        return subprocess.CompletedProcess(argv, 0, stdout=json.dumps({"count": 3, "bytes": 1234}), stderr="")

    assert r2_bytes_live("susqr2:b", runner) == 1234

    def failing(argv, **_kw):
        raise subprocess.CalledProcessError(1, argv)

    assert r2_bytes_live("susqr2:b", failing) is None


def test_audit_without_credentials_passes_and_prints_human_checklist(home, monkeypatch) -> None:
    monkeypatch.setattr(costs, "_gh_token", lambda _env: "")
    with respx.mock(assert_all_called=False), httpx.Client() as client:
        rows = costs.audit(home, {}, client, now=T0)
    text = render(rows)
    assert "RESULT: $0/month" in text
    assert "NOT upgraded to Pay-As-You-Go" in text
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
