# PLAN: Susquatch

*Written 2026-09-28 (T−3 days to open). Owner: Karthik. Status: M0 done, M1 in progress.*

This is the working plan. The brief in [`brief/`](brief/) is the input spec. [`brief/ERRATA.md`](brief/ERRATA.md) lists
where reality disagreed with it, and [`docs/API.md`](docs/API.md) is our reading of the official API.

---

## 0. The objective, restated so every design choice can be checked against it

**Maximize P(finish 1st).** Rank = SUSQie balance after every market resolves, so only the **resolved payoff of the
book held at the lock (2026-11-04 17:00Z)** matters. October P&L matters only as capital that multiplies the final bet.
Secondary goals: top 3, and a clean, legible trading record plus a write-up for SIG recruiting.

Three phases (SPEC §3): **October** (capital, calibration, signal) → **late October** (correlated scenario book) →
**election night** (rolling live data into mispriced legs). Direction is not an edge. *Latency and under-exposure are.*

---

## 1. Architecture

```
                   ┌────────────── Oracle Always Free (aarch64, 2 OCPU / 12 GB) ──────────────┐
                   │                                                                           │
 Kalshi / Poly ──► │  python-scheduler ── collectors ─► data/ (parquet)                        │
 (read-only)       │        │  calibrate ─► sim (C++ via pybind11)                              │
                   │        │  optimizer ─► tourney (C++ via pybind11)                          │
                   │        └─► targets/targets-<ts>-<hash>.json + current.json   (versioned)   │
                   │                              │ file                                        │
 Super Market ◄──► │  rust-live  ◄────────────────┘                                             │
 REST + Realtime   │   platform · gateway · book · oms · risk · strategies · journal            │
                   │   enr (SoS / Clarity pollers) ──► NATS enr.county_results ──► needle (C++)│
                   │                               ◄── needle.race_posteriors / national_shift │
                   │   watchdog · status pusher ───────────────────────────────┐                │
                   └─────────────────────────────────────────────────────────── │ ──────────────┘
                                                                                ▼
             Cloudflare (free): KV ◄─ status/kill ─► Pages dashboard (basic auth, Kill button)
                                 cron Worker ─► daily summary (dashboard + pinned issue in private *-reports repo)
                                 R2 ◄─ nightly backups (80 % quota guard)
```

**Hard boundaries** (from `LANGUAGES_AND_BOUNDARIES.md`):

- Rust runs everything that trades live. C++ runs the heavy modeling in three independent pieces. Python handles research and glue.
- **Rust and C++ are never linked.** They talk only through versioned files (targets) and NATS messages (needle).
  pybind11 (C++ ↔ Python) is the only in-process binding.
- One schema source: [`contracts/`](contracts/) holds protobuf, with the proto3 JSON mapping used for files.
  It generates Rust, C++, and Python types, and cross-language golden round-trip tests prove they agree byte for byte.
- Rust **refuses** stale, malformed, or hash-mismatched inputs. If needle goes silent, Rust stops acting on it and cancels what it placed.
- **Single-trader lease.** Only one process (the Python bridge or rust-live) may place orders at any moment.

---

## 2. What changed after reading the official docs (drives the build order)

1. **Realtime exists.** `relationships:violations:{tournamentId}` pushes constraint violations within about 250 ms.
   Competitor bots poll every 120–180 s. The constraints racer is **event-driven first and polling second**.
2. **GTD expirations exist.** Every order we send carries `expirationDate`, capped at `lock − 5 min`.
   A crashed bot's orders die on their own, which matters because nobody gets paged (no alerts, by design).
3. **ALL collateral.** Relationship-linked positions get guaranteed-payout-floor advances. Arbitrage bundles cost
   almost no buying power, and scenario sizing must use engine buying power.
4. **Tick is 0.005**, the book is a single YES-space book, and STP runs server-side.
5. **Field size is observable** (`memberCount`, leaderboard `total`), so `tourney` can fit on real N from day 1.

---

## 3. Build order & branches

Every milestone is a feature branch → PR (with CI) → merge into `main`. The table shows the plan. Checkmarks track reality.

| # | Branch | Contents | Needed by |
|---|---|---|---|
| 1 | `chore/scaffold` | layout, brief, API reference, errata, plan, tooling, CI skeleton | now |
| 2 | `feat/contracts` | protobuf schema → Rust/C++/Python codegen, golden fixtures, round-trip tests | before 3–9 |
| 3 | `feat/py-platform` | Python API client, **faithful mock exchange server**, paper broker | Sep 30 |
| 4 | `feat/bridge` | OMS, risk, journal, kill switch, lease, `measure_day1`, first trade, sniper, constraints racer | **Oct 1 12:00 ET** |
| 5 | `feat/rust-live` | platform / gateway / book / oms / risk / journal / strategies / targets / bus / `susq-live` | Oct 1–10 cutover |
| 6 | `feat/cpp-models` | `sim`, `tourney`, `needle` (NATS service) + pybind11 + tests | Oct 1–15 |
| 7 | `feat/research` | Kalshi + Polymarket collectors, fair value, calibration, optimizer, leaderboard telemetry, crowd inference | Oct 1–15 |
| 8 | `feat/election-night` | `enr` adapters (CA SoS, Clarity), synthetic 2022/2024 replays, drills, poll-close table | Oct 11–25 |
| 9 | `feat/ops` | Docker Compose, systemd, cloud-init, one-command restore, R2 backups, `make costs` | Sep 30 |
| 10 | `feat/dashboard` | Cloudflare Pages dashboard + Functions (auth, Kill) + cron Worker daily summary | Sep 30 |
| 11 | `docs/*` | README, runbooks, `.claude/commands`, write-up skeleton | continuous |

### Must be live by Oct 1 12:00 ET (ITERATION §1)

- [x] SuperMarket adapter built to the official docs (#9, #11). *Live verification needs the API key.*
- [x] gateway, OMS, risk, kill switch, journal (Python bridge, #10)
- [x] `measure_day1` → `reports/DAY1.md` (#10). *The live steps run on Oct 1 with the key.*
- [x] first qualifying trade (#10). **Armed only by "go live".**
- [x] constraints racer, realtime first with polling fallback (#10, #11, re-fire fix #12)
- [x] stale-order sniper (#10). *Trades only markets marked `verified` in the market map.*

Beyond the Oct 1 list, these landed early: rust-live (#11), C++ `sim`/`tourney`/`needle` (#13), research (#7),
election night (#8, #14), ops (#15), dashboard (#16).

---

## 4. Assumptions to verify (each has an owner check in `measure_day1` or a test)

| # | Assumption | How we verify | If wrong |
|---|---|---|---|
| A1 | Tournament slug is `midterm-elections` | `GET /tournaments` | Config value. The bridge refuses to trade if it can't resolve it. |
| A2 | Our key is org-bound to SIG with read+trade | `GET /account`, then a 1-share order | ACTION NEEDED: recreate the key |
| A3 | Markets are readable before open | `GET /tournaments/{slug}/markets` on Sep 29–30 | Build the market map at 12:00 ET instead |
| A4 | Rate limit ≥ 5 req/s sustained | Gentle ramp on `GET /account`, stopping at the first 429 | Decision table row 7: batch, realtime-first |
| A5 | Realtime works for participant keys | Mint a token, join `tournament:{id}` and `relationships:violations:{id}` | Polling only, at the fastest safe rate |
| A6 | Valuation price = last trade (affects leaderboard marks) | Compare `currentPrice` with the book and trades after the first fill | Recorded in DAY1.md |
| A7 | No per-market position limit at our sizes | Tiny probes. Record any limit error | Decision table row 3: broad basket |
| A8 | Constraint graph covers party-pair races | `GET /relationships?type=mutually_exclusive` | Our own pair checker covers the gap |
| A9 | The Sponsor quotes around Kalshi with size | Spread/depth census over all markets | Decision table rows 1–2 |

---

## 5. Pre-mortem: "It's Nov 5 and Susquatch failed. Why?"

| Failure story | Guard built in |
|---|---|
| Nobody had an API key at 12:00 ET on Oct 1 | To-do #1. The bridge polls for `.env` and starts itself the moment a key appears. |
| The server didn't exist yet | Same Docker Compose runs on the Mac (`make up`). A launchd plist is provided as a fallback. |
| A crash left resting orders that filled into the lock | GTD on every order (≤ 16:55Z), cancel-all at 16:55Z, time gate in risk, kill file |
| A retry double-placed an order | Deterministic idempotency keys persisted *before* send (write-ahead intents), replay-safe retries only for the documented codes |
| Cup↔Kalshi mapping inverted D/R, so the sniper bought the wrong side | Mapping rows need `verified: true`. A >25-point fair-vs-Cup gap is treated as a mapping error: blocked and flagged. The 2 % glitch cap applies. |
| Kalshi feed went stale and we quoted on old fair values | Fair-value staleness gate (60 s). Orders built on stale inputs are cancelled. |
| Bridge and Rust both traded after cutover | Lease file with TTL. Only the holder may call order endpoints (enforced in the gateway, not just by convention). |
| The bot got throttled for hammering | Realtime-first design, an adaptive limiter reading 429/Retry-After, and a budget that reserves order capacity |
| Election-night feed schema changed | Schema-validated adapters, cross-source agreement check, per-feed kill, and a manual website fallback |
| needle died silently mid-night | Heartbeat watchdog (10 s) and a seq-gap check. The executor stops and cancels needle-derived orders, resuming after N healthy messages. |
| Tied with someone at an identical balance | Idiosyncratic leg plus non-round quantities (tie hygiene) |
| Leaked Competition Data by publishing | `journal/`, `data/`, `reports/`, `state/` are gitignored. The reports repo gets aggregates only. A pre-publish scrub checklist is in the runbook. |
| Spent money | `make costs` audits every service against free-tier limits. No paid anything. Extra usage stays OFF. |

---

## 6. Risk policy (initial numbers, tuned via `experiments/`)

| Limit | Value | Where |
|---|---|---|
| Mode | **paper by default**. Live needs `SUSQ_MODE=live` **and** `ops/GO_LIVE` containing `go live` | gateway |
| Per-market gross | 5 % of equity (Oct), later set by the scenario book | risk |
| Per-cluster gross | national 25 %, region 15 %, state 10 %, House 20 % | risk |
| Total gross | 60 % of equity (Oct) | risk |
| Sniper mistake cost | < 1 % of bankroll per fill | sniper |
| Glitch trades (> 15 pts through fair) | ≤ 2 % of capital | risk |
| Loss-per-hour breaker | 3 % of equity (mark-to-market), which trips cancel-all + freeze | risk |
| Order reject breaker | > 10 rejects/min, which freezes | risk |
| Time gate | no new orders ≥ 16:55Z Nov 4, and every `expirationDate` ≤ 16:55Z | risk + OMS |
| Kill | `ops/KILL` file, dashboard Kill (KV), NATS `ops.kill`. All cancel-all and freeze. | everywhere |

---

## 7. Human steps (only Karthik can do these)

Ordered by deadline. The private `susquatch-reports` repo and its pinned **Daily summary** issue already exist.

1. **Before Oct 1:** register at sig.thesuper.market (student eligibility), confirm the email, accept the Terms and Rules, and opt in to recruiting contact.
2. **Before Oct 1:** create an API key (Settings → API Keys) with `read` + `trade`. Put it in `.env` as `SUSQ_API_KEY=…` and never paste it in chat.
   Then run `uv run susq-bridge record` once to snapshot the markets, so the map can be built before open.
3. **Before Oct 1:** decide where it runs on day one. The Mac (`make up`, colima) works now. The Oracle server is better but needs step 6.
4. **Before Oct 1:** run `/parse-rules`, paste Info-tab text into `state/info_tabs/<id>.md` for the risky markets, and mark rows `verified: true`.
   The sniper and scenario book trade only verified rows. The racer's NO-bundles don't need verification.
5. **Oct 1, before 12:00 ET:** say **"go live"** in a Claude Code session (`/go-live` runs the pre-flight). Without it, everything stays paper.
6. **This week:** sign up for Oracle Cloud Always Free (card hold only; **never** upgrade to Pay-As-You-Go), then `ops/cloud-init.yaml` + `ops/install.sh`.
7. **This week:** Cloudflare. Add `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` as GitHub Actions secrets (permissions in `docs/OPS.md` §4),
   enable R2, set the dashboard password secret, then merge-to-main deploys `susquatch.pages.dev`. Wrangler is already logged in locally, but deploying is your call.
8. **This week:** a fine-grained GitHub token (Issues: write, Contents: read on `susquatch-reports` only), stored as the Worker secret.
9. **Mid-October:** re-check the CA SoS media page for the 2026 test-window dates (see `brief/ERRATA.md` E18), and turn on Claude Code cloud routines for
   `/daily-review` (daily) and `/weekly-strategy` (Sundays), reading `susquatch-reports`, with **extra usage OFF**.
10. **Optional:** send the DDHQ student-access email (`docs/outreach/DDHQ_student_access.md`).

---

## 8. Timeline (ET)

- **Sep 28–30:** scaffold → contracts → Python platform + mock → bridge → ops + dashboard → Rust core in parallel.
- **Oct 1 12:00:** `measure_day1`, then the first trade, then the racer and sniper on the bridge. Apply the decision table.
- **Oct 1–10:** sim v1 calibrated to mids (±2–4 pts), first targets file, Rust paper run of 24 h or more, then lease cutover.
- **Oct 11–25:** needle v1, CA SoS captures, weekly synthetic replays, tourney fitted on real N, scenario-book draft.
- **Oct 26–Nov 2:** tranches, two dress rehearsals and one chaos drill, and the official poll-close table.
- **Nov 3 17:00 → Nov 4 11:55:** election night per the runbook. **No orders after 11:55.**
- **Nov 4 onward:** archive the journal privately, scrub, make the repo public, write up, email predictionscup@sig.com before publishing anything derived from platform data.
