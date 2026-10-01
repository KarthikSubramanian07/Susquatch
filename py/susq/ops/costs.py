"""`make costs`: prove every external service still costs the owner $0, or fail loudly.

For each service it prints the free limit, the usage it can measure (live APIs when credentials exist, the
local ops records otherwise), and the headroom. Where nothing is measurable it prints the budget implied
by our own configuration, so that a config change that would blow a free tier fails here before it
deploys. The server is the one thing not on a free tier: an Azure for Students VM paid from the one-time
$100 credit, with no card on file, so the guard is that the VM stays the size the runway was planned for.
The things only a human can confirm (above all, that Azure is **not** upgraded to Pay-As-You-Go) are
printed as a checklist every run.

Exit status: 0 = everything provably free, 1 = something costs money or would exceed a free limit.
Missing credentials never fail the audit; they downgrade that row to "unmeasured".
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
from collections.abc import Callable
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

import httpx

from susq.ops.common import OpsPaths, load_config, ops_setting, susq_home, utc_now

# Free-tier limits (checked 2026-09-28; OPS_FREE_AUTONOMY.md §A and each vendor's pricing page).
# Server: Azure for Students, Standard_B2pls_v2 (checked 2026-09-30). ~$34/month of the $100 credit
# (VM $0.04/h + static IP $0.005/h + 30 GB StandardSSD), so the credit lasts past Nov 30 at this size only.
AZURE_VM_SIZE = "Standard_B2pls_v2"
AZURE_VCPUS = 2
AZURE_MEM_GB = 4
AZURE_DISK_GB = 64
GH_ACTIONS_MIN_PER_MONTH = 2000
CF_REQUESTS_PER_DAY = 100_000  # Workers + Pages Functions, shared
CF_KV_WRITES_PER_DAY = 1_000
CF_KV_READS_PER_DAY = 100_000
B2_FREE_BYTES = 10_000_000_000  # Backblaze B2: 10 GB free, no card, uploads stop at the cap
CLAUDE_ROUTINES_PRO_PER_DAY = 5
DASHBOARD_REFRESH_S = 20  # dashboard/public/app.js polls /api/status at this cadence while open
WARN_FRAC = 0.8

HUMAN_CHECKLIST = (
    "Azure subscription is Azure for Students (no card, credit-funded); no Pay-As-You-Go upgrade "
    "(portal -> Subscriptions shows 'Azure for Students' and the remaining credit).",
    "Claude subscription: 'extra usage' is OFF (claude.ai -> Settings -> Usage).",
    "Cloudflare: account is on the Workers Free plan (no card on file).",
    "Backblaze B2: no card on file, so the 10 GB free tier stops uploads instead of billing.",
    "GitHub: Actions/Codespaces spending limit is $0 (Settings -> Billing and licensing -> Budgets).",
    "Super Market deposits: none needed (play money). Real-money venues: ACH only, never debit card.",
)


@dataclass
class Row:
    """One service line in the audit."""

    service: str
    free_limit: str
    usage: str
    status: str  # ok | warn | fail | unmeasured
    note: str = ""


def _frac_status(frac: float) -> str:
    if frac >= 1.0:
        return "fail"
    if frac >= WARN_FRAC:
        return "warn"
    return "ok"


# ── Server (Azure for Students) ────────────────────────────────────────────────


def _mem_gb() -> float | None:
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) / 1024 / 1024
    except OSError:
        return None
    return None


HOST_SERVICE = "Azure for Students B2pls_v2 (paid from $100 credit, no card)"


def host_row(
    *,
    system: str,
    machine: str,
    cpus: int | None,
    mem_gb: float | None,
    disk_gb: float | None,
) -> Row:
    """The server's shape. Anything bigger than B2pls_v2 burns the credit faster than the plan allows."""
    limit = f"{AZURE_VCPUS} vCPU / {AZURE_MEM_GB} GB, <= {AZURE_DISK_GB} GB disk"
    if system != "Linux" or machine not in {"aarch64", "arm64"}:
        return Row(HOST_SERVICE, limit, f"not on the server ({system}/{machine})", "unmeasured")
    usage = f"{cpus} vCPU / {mem_gb:.1f} GB RAM / {disk_gb:.0f} GB disk" if mem_gb and disk_gb else "?"
    over = (cpus or 0) > AZURE_VCPUS or (mem_gb or 0) > AZURE_MEM_GB + 0.5
    over |= (disk_gb or 0) > AZURE_DISK_GB
    note = f"bigger than {AZURE_VM_SIZE}: the $100 credit runs out before Nov 30" if over else ""
    return Row(HOST_SERVICE, limit, usage, "fail" if over else "ok", note)


# ── GitHub ─────────────────────────────────────────────────────────────────────


def github_rows(summary: dict[str, Any] | None, *, error: str = "") -> list[Row]:
    """From `GET /users/{u}/settings/billing/usage/summary` for the current month."""
    limit = f"{GH_ACTIONS_MIN_PER_MONTH} Actions min/month (private repos), $0 spend"
    if summary is None:
        return [Row("GitHub Actions", limit, "not measured", "unmeasured", error or "no billing token")]
    items = summary.get("usageItems") or []
    net = sum(float(i.get("netAmount", 0) or 0) for i in items)
    minutes = sum(
        float(i.get("grossQuantity", 0) or 0)
        for i in items
        if str(i.get("product", "")).lower() == "actions" and "minute" in str(i.get("unitType", "")).lower()
    )
    status = "fail" if net > 0 else _frac_status(minutes / GH_ACTIONS_MIN_PER_MONTH)
    note = f"net charges ${net:.2f} this month" if net > 0 else ""
    return [Row("GitHub Actions", limit, f"{minutes:.0f} min this month, net ${net:.2f}", status, note)]


def fetch_github_summary(client: httpx.Client, user: str, token: str, now: datetime) -> dict[str, Any]:
    resp = client.get(
        f"https://api.github.com/users/{user}/settings/billing/usage/summary",
        params={"year": now.year, "month": now.month},
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    resp.raise_for_status()
    data = resp.json()
    return data if isinstance(data, dict) else {}


# ── Cloudflare ─────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Design:
    """Daily request/write budgets implied by our own config (what we'd use at steady state)."""

    kill_polls: int
    pushes: int
    dashboard: int
    kv_writes: int
    kv_reads: int
    requests: int


def design_budget(config: dict[str, Any]) -> Design:
    kill_polls = int(86400 / float(ops_setting(config, "kill_poll_interval_s", 10)))
    push_interval = min(
        float(ops_setting(config, "status_push_interval_s", 150)),
        float(ops_setting(config, "status_push_interval_election_s", 120)),
    )
    pushes = min(int(86400 / push_interval), int(ops_setting(config, "status_push_daily_budget", 800)))
    dashboard = int(86400 / DASHBOARD_REFRESH_S)  # one tab left open all day
    kv_writes = pushes + 2 + 10  # + daily summary (latest + dated) + generous kill/unkill allowance
    kv_reads = kill_polls + pushes + dashboard * 2
    return Design(kill_polls, pushes, dashboard, kv_writes, kv_reads, kill_polls + pushes + dashboard + 1)


def cloudflare_rows(
    design: Design,
    *,
    kv_writes_today: int | None,
    paid_subscriptions: list[str] | None,
    billing_error: str = "",
) -> list[Row]:
    rows = []
    rows.append(
        Row(
            "Cloudflare Workers + Pages Functions",
            f"{CF_REQUESTS_PER_DAY:,} requests/day",
            f"~{design.requests:,}/day by design",
            _frac_status(design.requests / CF_REQUESTS_PER_DAY),
            f"kill polls {design.kill_polls:,} + pushes {design.pushes} + dashboard {design.dashboard:,}",
        )
    )
    measured = f"; {kv_writes_today} pushes today" if kv_writes_today is not None else ""
    rows.append(
        Row(
            "Workers KV writes",
            f"{CF_KV_WRITES_PER_DAY:,} writes/day",
            f"<= {design.kv_writes}/day by design{measured}",
            _frac_status(max(design.kv_writes, kv_writes_today or 0) / CF_KV_WRITES_PER_DAY),
            "status push budget + summary + kill",
        )
    )
    rows.append(
        Row(
            "Workers KV reads",
            f"{CF_KV_READS_PER_DAY:,} reads/day",
            f"~{design.kv_reads:,}/day by design",
            _frac_status(design.kv_reads / CF_KV_READS_PER_DAY),
        )
    )
    rows.append(
        Row("Cloudflare Pages", "500 builds/month, unlimited static", "direct upload, 0 builds", "ok")
    )
    if paid_subscriptions is None:
        rows.append(
            Row(
                "Cloudflare billing", "no paid subscriptions", "no billing token", "unmeasured", billing_error
            )
        )
    elif paid_subscriptions:
        rows.append(
            Row(
                "Cloudflare billing",
                "no paid subscriptions",
                ", ".join(paid_subscriptions),
                "fail",
                "paid plan found",
            )
        )
    else:
        rows.append(Row("Cloudflare billing", "no paid subscriptions", "none", "ok"))
    return rows


def fetch_cf_paid_subscriptions(client: httpx.Client, account: str, token: str) -> list[str]:
    """Names of account subscriptions with a non-zero price (needs a token with Billing:Read)."""
    resp = client.get(
        f"https://api.cloudflare.com/client/v4/accounts/{account}/subscriptions",
        headers={"Authorization": f"Bearer {token}"},
    )
    resp.raise_for_status()
    paid = []
    for sub in resp.json().get("result") or []:
        if float(sub.get("price") or 0) > 0:
            plan = sub.get("rate_plan") or {}
            paid.append(str(plan.get("public_name") or plan.get("id") or sub.get("id")))
    return paid


# ── Backups (Backblaze B2) ─────────────────────────────────────────────────────

B2_SERVICE = "Backblaze B2 (backups)"
B2_LIMIT = "10 GB free, no card (stops at the cap)"


def backup_row(bucket_bytes: int | None, source: str) -> Row:
    if bucket_bytes is None:
        return Row(B2_SERVICE, B2_LIMIT, "no backup yet", "unmeasured")
    frac = bucket_bytes / B2_FREE_BYTES
    return Row(
        B2_SERVICE,
        B2_LIMIT,
        f"{bucket_bytes / 1e9:.2f} GB ({frac:.0%}) via {source}; transactions free",
        _frac_status(frac),
        "backup.sh refuses uploads at 80%",
    )


def backup_remote(env: dict[str, str]) -> tuple[str, dict[str, str]] | None:
    """The rclone remote `backup_remote_init` in ops/lib.sh builds: any S3 bucket, configured by the
    `SUSQ_R2_*` variables (the names predate the move to Backblaze B2). None without credentials."""
    if override := env.get("SUSQ_BACKUP_REMOTE"):
        return override, {}
    if not (env.get("SUSQ_R2_ACCESS_KEY_ID") and env.get("SUSQ_R2_SECRET_ACCESS_KEY")):
        return None
    endpoint = env.get("SUSQ_R2_ENDPOINT", "")
    if not endpoint:
        if not env.get("CLOUDFLARE_ACCOUNT_ID"):
            return None
        endpoint = f"https://{env['CLOUDFLARE_ACCOUNT_ID']}.r2.cloudflarestorage.com"
    rclone_env = {
        "RCLONE_CONFIG_SUSQR2_TYPE": "s3",
        "RCLONE_CONFIG_SUSQR2_PROVIDER": env.get("SUSQ_R2_PROVIDER") or "Cloudflare",
        "RCLONE_CONFIG_SUSQR2_ACCESS_KEY_ID": env["SUSQ_R2_ACCESS_KEY_ID"],
        "RCLONE_CONFIG_SUSQR2_SECRET_ACCESS_KEY": env["SUSQ_R2_SECRET_ACCESS_KEY"],
        "RCLONE_CONFIG_SUSQR2_ENDPOINT": endpoint,
        "RCLONE_CONFIG_SUSQR2_NO_CHECK_BUCKET": "true",
    }
    return f"susqr2:{env.get('SUSQ_R2_BUCKET') or 'susquatch-backups'}", rclone_env


def bucket_bytes_live(remote: str, runner: Callable[..., subprocess.CompletedProcess[str]]) -> int | None:
    """`rclone size --json <remote>` when rclone and bucket credentials are present."""
    if not shutil.which("rclone"):
        return None
    try:
        out = runner(
            ["rclone", "size", "--json", remote], capture_output=True, text=True, timeout=60, check=True
        )
        return int(json.loads(out.stdout)["bytes"])
    except (subprocess.SubprocessError, OSError, ValueError, KeyError):
        return None


# ── Claude ─────────────────────────────────────────────────────────────────────


def claude_row() -> Row:
    per_day = 1 + 1 / 7  # /daily-review daily + /weekly-strategy weekly
    return Row(
        "Claude Code routines",
        f"subscription cap (Pro {CLAUDE_ROUTINES_PRO_PER_DAY}/day, Max 15/day)",
        f"~{per_day:.1f} runs/day by design",
        _frac_status(per_day / CLAUDE_ROUTINES_PRO_PER_DAY),
        "no API keys, no runtime LLM calls",
    )


# ── orchestration ──────────────────────────────────────────────────────────────


def _read_json(path: Path) -> dict[str, Any] | None:
    try:
        data = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def _gh_token(env: dict[str, str]) -> str:
    if token := env.get("SUSQ_GITHUB_BILLING_TOKEN") or env.get("GH_TOKEN"):
        return token
    if shutil.which("gh"):
        try:
            out = subprocess.run(
                ["gh", "auth", "token"],  # noqa: S607 - fixed argv, resolved via PATH on purpose
                capture_output=True,
                text=True,
                timeout=5,
                check=True,
            )
            return out.stdout.strip()
        except (subprocess.SubprocessError, OSError):
            return ""
    return ""


def audit(home: Path, env: dict[str, str], client: httpx.Client, *, now: datetime) -> list[Row]:
    config = load_config(home)
    paths = OpsPaths(home)
    rows = []

    disk = shutil.disk_usage("/").total / 1e9
    rows.append(
        host_row(
            system=platform.system(),
            machine=platform.machine(),
            cpus=os.cpu_count(),
            mem_gb=_mem_gb(),
            disk_gb=disk,
        )
    )

    user = env.get("SUSQ_GITHUB_USER", "KarthikSubramanian07")
    token = _gh_token(env)
    if token:
        try:
            rows += github_rows(fetch_github_summary(client, user, token, now))
        except (httpx.HTTPError, ValueError) as exc:
            rows += github_rows(None, error=f"billing API: {type(exc).__name__} (token needs Plan: read)")
    else:
        rows += github_rows(None)

    push = _read_json(paths.push_record) or {}
    kv_today = int(push["count"]) if push.get("date") == now.strftime("%Y-%m-%d") else None
    backup = _read_json(paths.backup_record) or {}
    bucket_bytes, bucket_source = None, ""
    if (live := backup_remote(env)) is not None:
        remote, live_env = live

        def runner(argv: list[str], **kw: Any) -> subprocess.CompletedProcess[str]:
            return subprocess.run(argv, env={**os.environ, **live_env}, **kw)  # noqa: S603

        bucket_bytes, bucket_source = bucket_bytes_live(remote, runner), "rclone size"
    if bucket_bytes is None and backup.get("bucket_bytes") is not None:
        bucket_bytes, bucket_source = int(backup["bucket_bytes"]), "last backup record"

    paid: list[str] | None = None
    billing_error = ""
    cf_billing = env.get("SUSQ_CF_BILLING_TOKEN", "")
    if cf_billing and env.get("CLOUDFLARE_ACCOUNT_ID"):
        try:
            paid = fetch_cf_paid_subscriptions(client, env["CLOUDFLARE_ACCOUNT_ID"], cf_billing)
        except (httpx.HTTPError, ValueError) as exc:
            billing_error = f"subscriptions API: {type(exc).__name__}"
    rows += cloudflare_rows(
        design_budget(config),
        kv_writes_today=kv_today,
        paid_subscriptions=paid,
        billing_error=billing_error,
    )
    rows.append(backup_row(bucket_bytes, bucket_source))
    rows.append(claude_row())
    rows.append(
        Row("Data sources", "free/public only", "platform API, Kalshi/Polymarket public, SoS feeds", "ok")
    )
    return rows


MARK = {"ok": "ok  ", "warn": "WARN", "fail": "FAIL", "unmeasured": "  ? "}


def render(rows: list[Row]) -> str:
    w_service = max(len(r.service) for r in rows)
    w_limit = max(len(r.free_limit) for r in rows)
    lines = [f"{'':4}  {'service':<{w_service}}  {'free limit':<{w_limit}}  usage"]
    for r in rows:
        note = f"  ({r.note})" if r.note else ""
        lines.append(
            f"{MARK[r.status]}  {r.service:<{w_service}}  {r.free_limit:<{w_limit}}  {r.usage}{note}"
        )
    lines.append("")
    lines.append("Human checks (only you can confirm these):")
    lines += [f"  [ ] {item}" for item in HUMAN_CHECKLIST]
    failed = [r.service for r in rows if r.status == "fail"]
    lines.append("")
    lines.append(f"RESULT: FAIL, would cost money: {', '.join(failed)}" if failed else "RESULT: $0/month")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Free-tier cost audit")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args(argv)
    with httpx.Client(timeout=10.0) as client:
        rows = audit(susq_home(), dict(os.environ), client, now=utc_now())
    if args.json:
        print(
            json.dumps(
                {"rows": [asdict(r) for r in rows], "human_checklist": list(HUMAN_CHECKLIST)}, indent=2
            )
        )
    else:
        print(render(rows))
    return 1 if any(r.status == "fail" for r in rows) else 0


if __name__ == "__main__":
    sys.exit(main())
