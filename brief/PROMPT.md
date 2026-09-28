# Susquatch — Claude Code build prompt

## STEP 0 — Read my user-level CLAUDE.md first (overrides everything)

Before doing anything else, read my user-level memory file at `~/.claude/CLAUDE.md` and any files it imports. **Its instructions override everything in this prompt and in the brief wherever they conflict.** If it does not exist, say so in one line and continue.

## STEP 0.5 — Hard constraint: no Anthropic API spending (overrides all LLM text below)

I use my **Claude subscription only**. I will not create an Anthropic API key or pay for API usage. This section overrides every mention in this prompt and the brief of model IDs, the Claude Agent SDK, the Batch API, per-agent token budgets, or "LLM cost caps".

- **Runtime code is LLM-free.** No Anthropic API calls, SDK billing, or API keys anywhere in the system.
- **What replaces the automated LLM jobs:**
  - Info-tab resolution-rule parsing becomes `/parse-rules`, run once at the start and again whenever new markets appear. It writes structured JSON that I spot-check. Code validates that JSON against the schema.
  - Anomaly triage becomes deterministic flags in the daily summary, reviewed with `/daily-review`.
  - The research log and write-up are drafted with `/draft-writeup` from the scrubbed log. Code keeps the log itself.
  - Election night was already LLM-free. Keep it that way.
- **Claude Code as the on-demand analyst.**
  - Code generates **review packets** (markdown + CSV) under `reports/packets/`.
  - Create custom slash commands in `.claude/commands/` (e.g. `/daily-review`, `/weekly-strategy`, `/parse-rules`, `/draft-writeup`). Each one tells Claude Code which packet to read, what to analyze, and which file to write.
  - I run these in an interactive Claude Code session under my subscription.
  - Their outputs are files: reports, proposed config diffs, or experiment drafts. They **never place orders or change live limits directly.** Any change goes through the experiment registry and my approval.
- **Scheduled Claude reviews use Claude Code cloud routines, included in my subscription** within its daily run caps. **Extra usage stays OFF.** Routines read only summary packets from a private `*-reports` repo and write proposals. They never trade or change live limits.

## STEP 0.6 — Zero cost and autonomy (overrides everything below)

Read `brief/OPS_FREE_AUTONOMY.md` now; it overrides this prompt and every other brief file.
- **Everything except my Claude subscription must cost $0/month.** That means the Oracle Cloud Always Free ARM server (2 OCPU / 12 GB since June 2026, never Pay-As-You-Go), GitHub Free, **Cloudflare free** (a password-protected **pages.dev** dashboard with a Kill button, one cron Worker that posts the daily summary to the dashboard and as a comment on a pinned issue in the private reports repo (GitHub emails me; **no Telegram or chat apps**) and detects a dead server, R2 backups with a quota guard; no domain purchase), and free data only. **No real-time alerts: only one daily summary for this project (each project sends its own).** If something can't be done for free, stop and tell me. Build `make costs` and a one-command restore.
- I will step in when needed during the contest. No alerts (daily summary only), daily and weekly cloud-routine reviews, and Competition Data kept off GitHub.

## STEP 1 — Read the official API docs, then the brief, before writing any code

**Official API docs (read these first, in full): https://sig.thesuper.market/api/v1/docs**

Fetch the page yourself: try your web fetch tool first, then `curl -sL -A "Mozilla/5.0" https://sig.thesuper.market/api/v1/docs`, and follow any OpenAPI/JSON link on it. Save what you get to `brief/api/official_docs.*`. These official docs override `brief/API_NOTES.md` wherever they disagree. Also read the product guide at https://sig.thesuper.market/docs and the rules at https://predictionscup.com/rules/.

Then read the brief:

- `brief/RESEARCH_cup.md`: verified rules, platform facts, the midterm landscape, simulator inputs, election-night data, late-count races, tournament theory, and the unknowns list.
- `brief/SPEC_susquatch.md`: architecture, the three strategy phases, the decision table, tests, and the timeline.
- `brief/LANGUAGES_AND_BOUNDARIES.md`: **the authority on languages and process boundaries.** Rust runs everything live; C++ does modeling as three independent pieces, connected only by versioned files and a message bus, **never compiled together**; one shared schema with cross-language golden tests; Rust refuses stale or malformed inputs; and a Python bridge for the first days of October with lease-based handoff to Rust.
- `brief/ITERATION_AND_IMPROVEMENTS.md`: **the MVP-first checklist for Oct 1, six upgrades (constraints racer, leaderboard telemetry feeding an empirical tournament model, crowd-position inference, tie-breaker hygiene, the lock-time rule, weekly election-night drills), and the daily iteration loop. It overrides any conflicting text in this prompt or the spec.**
- `brief/API_NOTES.md`: the API surface (base URL, Bearer auth, endpoints, the order body, response fields), reverse-engineered from a competitor's working public client. **Unofficial: verify it.**
- `brief/api/` (may be empty): I will drop saved copies of the official API docs here if you can't fetch them.

Read everything. The official API reference is **https://sig.thesuper.market/api/v1/docs**. Fetch it yourself with `curl -sL` and a browser User-Agent (some automated fetchers get blocked by robots rules), and look for an OpenAPI/JSON link on the page. Once I've given you an API key, confirm each endpoint in `API_NOTES.md` with read-only calls (`GET /account`, `GET /tournaments`). Probe both hosts named in the notes (`www.thesuper.market` and `sig.thesuper.market`). If you can't read the official docs, ask me to save the page into `brief/api/`. Never invent endpoints. Code against a `PlatformAdapter` trait with a mock implementation, and keep the real implementation behind it.

## Mission

Build **Susquatch**, an automated system for the Susquehanna Predictions Cup (Midterm Elections competition on The Super Market). It is built to **maximize P(1st place)**, since first prize is $30k of a $37.5k pool, and to leave a clean, impressive trading record and write-up for SIG recruiting.

**Critical dates (ET):**
- Trading opens **Oct 1, 2026, 12:00pm**.
- Election Day is **Nov 3**.
- Trading locks **Nov 4, 2026, 12:00pm**. All trades must be received before then.
- Rank = SUSQie balance after **all markets resolve**. That can be weeks later for slow-counting races.

Today is Sept 27, so the pre-Oct-1 checklist is the priority.

## Non-negotiable rules (from the Official Rules)

- One account only, strictly individual. **No coordination with anyone and no trading against friends' accounts.** No wash or self trades (enforce self-match prevention in the OMS).
- Bots are allowed without limit via the API, subject to rate and position limits. The platform may throttle or remove bots and may void or reverse trades. Never size a trade that exploits an obvious glitch above 2% of capital.
- You must make ≥1 trade to be ranked.
- No trading on MNPI. That includes anything connected to a campaign I work or volunteer for.
- The Sponsor owns all Competition Data: orders, trades, positions, API logs, and derivatives of them. **Keep raw platform logs private forever.** The repo stays **private until Nov 4 noon ET**. After that, publish code, methodology, the simulator, external-data backtests, and aggregate results only.

## Hard constraints

- **Languages:**
  - **Rust (tokio) runs everything that trades live:** platform connection, gateway, book, OMS, risk, all strategies, the election-night results feeds (`enr`), and the executors.
  - **C++20 does the heavy modeling, as three independent pieces.** `sim` (correlated race Monte Carlo) and `tourney` (competitor/field model) are C++ libraries called **only from Python** via pybind11. Python writes a versioned target file that Rust reads. `needle` (election-night vote model) is its **own C++ program** on the message bus: it receives county results from Rust, publishes race posteriors back, and has pybind11 bindings for replays.
  - **No FFI, C ABI, or linking between Rust and C++, ever.** One shared schema (`contracts/`, protobuf + JSON Schema) generates the types for all three languages, with cross-language golden round-trip tests. Rust rejects stale or malformed inputs and stops acting on `needle` if it goes silent. Full details are in `brief/LANGUAGES_AND_BOUNDARIES.md`.
  - **Python bridge (first days of October only):** `measure_day1`, the first qualifying trade, the sniper, and the constraints racer. It hands off to Rust under a single-trader lease once Rust passes its tests.
  - Python for research, calibration, data collection (Kalshi and Polymarket read-only, polls), the scenario optimizer, and replays.
- **LLM agents:** Claude, off the hot path, cost cap $40 for the whole competition. Uses: parse each market's Info-tab resolution rules into structured JSON (source, runoff/recount/RCV/independent handling), triage anomalies, and write the research log. **LLMs never place orders.**
- **Risk:** caps per market, per cluster (national, region, state, House), and gross. Loss-per-hour circuit breaker. "Locked" scenario legs that strategies cannot sell. Kill switch via file plus a Kill button on the pages.dev dashboard. Paper mode by default. Live trading only after I say "go live" in this session.
- **Journaling:** every request and response to the platform goes to a private append-only journal (this is Competition Data, so it never gets committed).
- **Secrets:** API key in `.env` or the OS keychain. gitleaks pre-commit hook.

## Build order

1. **M0 (today): Plan.** `PLAN.md` covering the architecture, assumptions to verify, and the human steps I must do (register, opt in to recruiting contact, create an API key with read+trade scopes, share the API docs). Show me the plan, then proceed without waiting unless you're blocked.
2. **M1 (by Sept 30): Core, MVP-first.** Ship the "must be live by Oct 1" list in `ITERATION_AND_IMPROVEMENTS.md` before anything else. Everything below can follow.
   - **Python bridge** (only what must be live on Oct 1; see `LANGUAGES_AND_BOUNDARIES.md`), built first. Then the Rust `gateway` with an adaptive rate limiter (reads 429 and Retry-After), idempotent client order IDs, a journal, `oms` with a reconcile loop, and `risk` with the kill switch.
   - The `SuperMarket` adapter implementing the endpoints in `API_NOTES.md`: tournaments, paginated markets, bulk `/exchanges/prices` (≤100 IDs per call), orderbook, `/relationships/constraints`, positions, leaderboard, and `POST /orders` with deterministic idempotency keys. Prices are decimals 0–1 at the API boundary and integer ticks internally.
   - Kalshi and Polymarket read-only collectors plus a fair-value service (market-anchored).
   - A minimal stale-order sniper and a two-party constraint checker, in paper mode against the mock.
3. **M2 (Oct 1, first hour): Measurement.** Run `measure_day1`, which records:
   - which API host works, tick size, payout units, price bounds, and NO mechanics (separate book or complement);
   - whether cancel and open-order endpoints exist;
   - order types, fees, rate-limit responses, and position-limit probes at tiny size;
   - the Sponsor's spread and depth in every market;
   - how account value marks open positions;
   - leaderboard size.
   Write `reports/DAY1.md` and apply the decision table in the spec.
4. **M3 (Oct 1–10):**
   - Place the first qualifying trade.
   - Start market making or sniping as the decision table dictates.
   - Parse all Info tabs with the LLM, then have me spot-check.
   - C++ `sim` v1 (pybind11, called from Python) calibrated to real-money mids; first versioned target file consumed by Rust.
   - Cut over from the Python bridge to Rust once Rust passes its tests (lease handoff).
5. **M4 (Oct 11–25):**
   - `needle` v1 plus state results adapters (California SoS JSON first). Capture every CA SoS test window (dates in the brief are unverified; confirm them).
   - Synthetic replays of the 2022 and 2024 election nights.
   - `tourney` using observed field data.
   - Draft the scenario book.
6. **M5 (Oct 26–Nov 2):**
   - Put the scenario book on in tranches.
   - Two full election-night dress rehearsals plus one chaos test (kill feeds and the network mid-replay).
   - Finalize poll-closing times from official sources.
7. **M6 (Nov 3, 5pm ET to Nov 4, 11:55am ET):** Live election night per the runbook in the spec. No orders after 11:55am ET.
8. **M7 (Nov 4 onward):** Archive the journal privately. Scrub the repo. Publish the code and write-up.

## Iteration

SUSQies are play money, so iterate daily. The daily ops report ends with the top 3 proposed changes; I choose, you ship them and log each in `experiments/`. Re-run scenario selection weekly using fitted leaderboard data.

## Operating rules for you (Claude Code)

- Work autonomously. Ask me only at blocking human steps, and batch those questions.
- Live docs and observed platform behavior beat the brief. Log any disagreement in `brief/ERRATA.md`.
- Never fabricate numbers. Every figure in a report comes from code you ran.
- Keep a timestamped `research_log.md` (a scrubbed version will feed the write-up).

## Definition of done

- The paper system runs end to end against the mock and, once live, against the real platform.
- `measure_day1` produces `DAY1.md`.
- `sim`, `needle`, and `tourney` have unit tests, and `sim` is calibrated to within 2–4 points of market mids.
- The election-night pipeline passes two replays and one chaos test.
- A runbook exists for Nov 3–4.
- A publishable write-up draft exists that contains no Competition Data beyond public aggregate results.
