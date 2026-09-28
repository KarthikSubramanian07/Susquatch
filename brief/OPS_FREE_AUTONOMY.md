# OPS_FREE_AUTONOMY — Susquatch

This file overrides any conflicting text in PROMPT.md and every other brief file.

## A. Zero-cost rule (overrides everything)

My only allowed recurring cost is my **Claude subscription**. Every other component must be **$0/month**. If something can't be done for free, stop and tell me instead of adding a paid service. Build a `make costs` check that lists every external service and its free-tier headroom.

| Component | Free choice | Limits and caveats you must design around |
|---|---|---|
| Server | **Oracle Cloud Always Free, Ampere A1 (ARM64)** in a US East home region | Since **June 15, 2026** the Always Free A1 allowance is **2 OCPU / 12 GB RAM** (it used to be 4/24), plus 200 GB block/boot storage. Signup needs a card for a $1 verification hold. **Never upgrade to Pay-As-You-Go.** Capacity can be scarce; retry, or try another availability domain. Oracle's docs have said **idle Always Free instances can be reclaimed** (low CPU/network/memory over 7 days). Verify the current policy, and design so a reclaim is survivable (see "Rebuild in minutes"). Build and run everything for **aarch64**. |
| Fallback server | An old laptop / Raspberry Pi / home PC I already own | Same deploy scripts (Docker Compose), so switching hosts takes one command. |
| Code hosting | GitHub Free (public repos; private repos for the Cup until Nov 4 and for review packets) | Private-repo Actions minutes are limited, so keep CI on private repos light: build and test on the server, and run heavy CI only on public repos. |
| Scheduled Claude reviews | **Claude Code cloud routines** (included in my subscription) | Daily run caps (Pro 5/day, Max 15/day) and they count toward my usage limits. **Keep "extra usage" turned OFF** so nothing is ever billed beyond the subscription. Routines only read summary packets from a private `*-reports` repo. They never trade or change live limits. |
| Dashboard | **Cloudflare Pages** site on a free `*.pages.dev` subdomain, plus Pages Functions and Workers KV (free) | The server pushes a small status JSON to KV every few minutes (equity, stage, positions, last trades, health, heartbeat). The Pages site shows it behind a password (basic auth in a Pages Function). It also hosts the **controls**: a **Kill** button and, for batman only, an optional approve/reject list for manual intents. Each writes a flag to KV that the server polls. No domain purchase: `pages.dev` is free. |
| Daily summary sender | **One small Cloudflare Worker with a daily cron trigger** (free plan). **No Telegram and no chat apps at all.** | Once a day it reads KV and posts **this project's own daily summary** in two places: (1) the pages.dev dashboard ("Today's summary" at the top), and (2) a new comment on a pinned "Daily summary" issue in this project's **private `*-reports` GitHub repo**. GitHub then emails me and notifies the GitHub mobile app, both free with no domain needed. The Worker holds a fine-grained GitHub token (issues:write on that one repo only) as a Worker secret. Because it runs on Cloudflare, not my server, it still posts "server silent since X" if the server dies. Config `push=false` turns off the GitHub comment and leaves the dashboard only. |
| Backups / archive | **Cloudflare R2 free tier (10 GB)** | A usage guard stops uploads at 80% of the free quota. Compress Parquet (zstd). Retention: raw deltas are compacted into daily snapshots. |
| Data | Only free sources (listed per project below) | No paid feeds (no AP, no paid DDHQ, no paid market data). |

**Rebuild in minutes.** All infrastructure lives in code in the repo: cloud-init plus an install script and Docker Compose, with Terraform for Oracle optional. Secrets live outside the repo. Nightly backups of state and data go to the free archive. The target is a fresh server fully restored with one command in under 30 minutes. Test this restore once during the build.

**Trading fees are not infrastructure costs,** but minimize them. Use ACH for deposits and withdrawals (free); never use debit card deposits.

## B. No alerts, one daily summary per project (overrides every "alert me" / "notify" / "NEEDS-HUMAN" / "Telegram" / "remote command" / "weekly summary message" anywhere in this package)

- **There are no real-time alerts.** The only outbound message is **one daily summary for this project**. Each of my three projects (calshi, batman, Susquatch) sends its own summary; they are never combined. It includes:
  - an **ACTION NEEDED** block at the top, empty on most days (e.g. Robinhood re-login needed, credentials failed, restore failed, free quota nearly used);
  - this project's P&L, stage, notable events, freezes/demotions/promotions, and health.
- **Anything that used to "alert me" is recorded and appears in this project's next daily summary.** The systems must therefore **fail safe on their own and not wait for me**:
  - kill/demote/freeze automatically;
  - protective broker-side stops (batman);
  - cancel orders based on stale inputs (Susquatch, calshi).
- **Remote control** lives on the password-protected pages.dev dashboard (Kill button). File-based kill (`ops/KILL`) still works over SSH.
- The weekly Claude routine's review report is linked from the daily summary. It isn't a separate message.

## C. Self-healing (so it doesn't need me)

- systemd/Compose restart policies, with automatic reconnect and resubscribe for every feed.
- A reconcile loop repairs local state from the venue.
- Automatic demotion or freeze on anomalies instead of paging me.
- A daily self-check (disk, quotas, clock drift, cert/key expiry dates, free-tier headroom). It fixes what it can and escalates only the rest.

## D. Susquatch (I will step in when needed)

This project stays more interactive: I'm around during the contest and on election night. It still follows the zero-cost rule and runs on the same free server.
- **No alerts.** Susquatch sends **its own daily summary**. On election night (Nov 3–4) I'll watch the pages.dev dashboard myself (and optionally local Grafana via SSH tunnel). The engine must fail safe without me: it stops acting on stale inputs, hits its breakers, and cancels all orders by 11:55 ET.
- **Scheduled reviews:** one cloud routine daily (`/daily-review`) and one weekly (`/weekly-strategy`), reading only the private reports repo. Keep **raw platform logs (Competition Data) off GitHub entirely.** Only aggregated packets go to the private reports repo.
- **Free data only:** the platform API, Kalshi/Polymarket public prices, free poll aggregates and ratings pages that allow access, official state election sites (CA SoS JSON feeds and test windows), and public historical county results. Paid AP/DDHQ feeds are out. If DDHQ offers student access at no cost, the agent drafts the request and I send it.
- **Server load:** 2 OCPU / 12 GB is shared with calshi. On election night, give Susquatch priority via cgroup CPU weights, and throttle calshi's non-critical jobs (backfills, research) from Nov 3 18:00 ET to Nov 4 12:00 ET.
