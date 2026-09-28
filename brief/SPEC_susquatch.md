# SPEC — Susquatch (SIG Predictions Cup bot)

> **Precedence:** `ITERATION_AND_IMPROVEMENTS.md` in this folder overrides any conflicting timeline, money gate, or build-order text in this spec.

## 1. Objective function

Maximize **P(finishing 1st)**, not expected balance. Rank depends only on the **resolved** payoff of what you hold at the Nov 4, 12:00pm ET lock (after all markets resolve). October mark-to-market doesn't matter except as capital. Secondary goals: finish top 3, and produce a clean trading record plus a write-up for recruiting.

## 2. Architecture

```
susquatch/  (PRIVATE until Nov 4 noon ET)
  Cargo.toml (workspace)
  crates/
    platform/        # PlatformAdapter trait; impl SuperMarket (real; endpoints per brief/API_NOTES.md) + Mock (paper/replay)
    gateway/         # REST(+WS if exists), adaptive limiter (429/Retry-After), journaling
    book/            # L2 per market, staleness timestamps
    oms/             # order state machine, idempotent client IDs, reconcile, self-match prevention
    risk/            # caps per market / cluster(national, region, state, House) / gross; loss-per-hour
                     # breaker; locked scenario legs; kill switch (file + remote)
    strategies/      # mm, sniper, pair_constraint, scenario_allocator (reads targets), en_executor
    enr/             # election-night pollers: CA SoS JSON, Clarity/Scytl ENR, other SoS; schema adapters,
                     # dedupe, cross-source consistency, batch-type tagging
    contracts/       # generated Rust types from /contracts (no hand-written duplicates)
    targets/         # reader/validator for versioned target files (staleness, hash, schema checks)
    bus/             # NATS client: publish enr.county_results; subscribe needle.* ; heartbeat watchdog
  contracts/         # SINGLE schema source: protobuf (bus) + JSON Schema (target files); codegen for Rust/C++/Python;
                     # golden fixtures + cross-language round-trip tests
  cpp/               # NO linkage with Rust. pybind11 to Python only.
    sim/             # library: correlated race Monte Carlo (t-copula, systematic-miss mixture, House aggregation)
    tourney/         # library: field simulation -> T(ω) distribution, P(1st) for candidate books
    needle/          # standalone service on the bus: county results in -> race posteriors + national shift out;
                     # plus pybind11 bindings for replays
    bindings/        # pybind11 modules for sim, tourney, needle
  bridge_py/         # Python bridge for the first days of Oct (measure_day1, first trade, sniper, constraints racer);
                     # single-trader lease; archived after cutover
  py/susq/
    collectors/      # Kalshi + Polymarket read-only prices (Parquet), polls, ratings
    calibrate/       # fit sim to market mids
    optimizer/       # scenario selection + construction
    replay/          # synthetic election nights from 2022/2024 county results + batch ordering
    agents/          # Info-tab rules parser, anomaly triage, research-log writer (cost-capped)
    reports/         # DAY1.md, daily ops report
  journal/           # gitignored: raw platform req/resp (Competition Data)
  data/              # gitignored
  ops/               # docker-compose, systemd, grafana, runbook
```

Language and process boundaries are defined in `LANGUAGES_AND_BOUNDARIES.md`: separate supervised processes (`rust-live`, `needle`, `nats`, `python-scheduler`, watchdog), connected only by versioned files and the bus. Run on a cloud VM in us-east with a watchdog. Keep a laptop as a manual fallback: the website can still be traded by hand.

## 3. Phase strategies

### Phase 1 — Oct 1–~20: capital, calibration, signal

- **Fair value:** `fair = 0.8 × real-money mid (Kalshi/Polymarket) + 0.2 × sim`. Stay anchored, because SIG is better informed than your model.
- **Market making:**
  - `spread = max(2 ticks, sponsor_spread + 1 tick)`.
  - Skew quotes by inventory and by cluster exposure.
  - Cancel everything on any real-money move of 2 points or more.
- **Sniper:** take student orders that sit ≥3–4 points through fair after ticks and fees. Size each fill so that a mistake costs <1% of bankroll.
- **Constraint trades (a crowded speed race):** the platform itself publishes `GET /relationships/constraints`, with `violationAmount` and `suggestedCorrectiveTrades`, to every API user, and at least one competitor bot trades it on a 120–180 s cycle. Poll it at the fastest safe rate, act within one cycle, size small, and don't count on it as a durable edge. Independently verify every suggested trade against the Info-tab rules (independents, RCV).
- **Your own constraint checks:**
  - In two-party races only, and only if the market's Info tab guarantees exactly one YES: if YES(D) + YES(R) < 100 − ticks, buy both.
  - **Do not apply this where an independent can win** (e.g., Nebraska Senate, Osborn (I)).
  - Chamber-vs-race relative value uses the sim's joint distribution. Treat these as risk trades, not arbitrage.
- **Expected result:** +3% to +25% if student flow exists; about 0% if the Sponsor sits at the touch.

### Phase 2 — Oct 21–Nov 2: scenario book

- **Scenario grid:** each scenario S = (national-shift decile) × (cluster shocks) from `sim`.
- **Score each S by:**
  - P(S), calibrated to real money;
  - the field's exposure to S, from Cup-vs-Kalshi drift and leaderboard behavior;
  - the multiple M_S from concentrating in the cheapest legs consistent with S;
  - P(M_S > T | S), from `tourney`.
  - **Maximize P(S) × P(M_S > T | S).**
- **Construction:** hold cheap, correlated legs that all pay in S. Early-reporting legs double as election-night rolling fuel.
- **Sizing:** 50–70% of capital in the scenario book, 30–50% held as election-night dry powder. Move to 80–90% in the book if there's no evidence of stale depth.
- **Tie-avoidance:** include at least one idiosyncratic leg, because everyone starts at exactly 100,000.
- **Direction is not an edge.** G. Elliott Morris found that correcting for predicted poll bias helped in only 9 of 14 cycles. Choose the scenario the **Cup misprices relative to real money** and that the **field is least exposed to**.

### Phase 3 — Nov 3, 7pm ET to Nov 4, 11:55am ET: election-night engine

- **Ingest:** state SoS/ENR JSON (CA SoS first; Clarity/Scytl pages), plus DDHQ if accessible. Record Kalshi/Polymarket ticks as the reference clock.
- **Model:** `needle` estimates each county's remaining vote (expected turnout − reported), by vote type (mail/early/Election-Day/late provisional). Priors come from 2022/2024 batches plus the county's reported batches. It propagates correlated county errors, then updates the **national-shift posterior**, which moves every not-yet-reporting race through `sim`'s correlation structure.
- **Buy rule:** q − ask ≥ max(8 pts, 3 × model SE), **and** corroboration from ≥2 independent county batches or a same-direction Kalshi move.
- **Roll:** sell appreciated legs as the Cup approaches q. Roll proceeds into later-closing races where q ≫ price.
- **Before lock:** position in markets that won't be called by noon Nov 4 (CA House, Alaska RCV, AZ/NV close races, WA/OR mail-heavy seats), using batch-type late-count estimates by county rather than folklore about which way late ballots shift.
- **Kill switches:** feed outage; feed disagreement above threshold; signs the Sponsor is voiding trades; order rejects per minute > N. **No orders after 11:55am ET.**

## 4. Decision table (apply after `measure_day1`)

| Observation (Oct 1–7) | Action |
|---|---|
| Sponsor quotes ≤2¢ wide around Kalshi with size | Scale MM down to sniper only. Shift ~70% of engineering to election night. |
| Sponsor absent or wide; students set prices | Run full MM around Kalshi fair (+10–25% expected). |
| Small per-market position limits | Build the scenario book as a broad correlated basket, and model the multiple at the basket level. |
| 1¢ ticks and many contracts at 1–5% | Raise T estimates. Boost election-night rolling capacity. |
| Cup prices drift more D than Kalshi | The field is long D. Favor the "R beats polls" scenario. Reverse this if the drift goes the other way. |
| Top leaderboard balances ≥3× by mid-Oct | Investigate mark manipulation or illiquid marks. Don't chase mark-to-market ranks. |
| Rate limits tight (<5 req/s) | Prioritize streaming (if any). Batch reads, cancel sparingly, reserve capacity for election night. |

## 5. C++ simulator parameterization (`sim`)

- **Statewide margin error:** e = N + R_region + S_state + ε_race.
  - N: Student-t (ν≈4–6), SD ≈ 2.5–3.0 pts of margin.
  - R_region: SD ≈ 1.5, over Northeast / Midwest / South / Mountain-West.
  - S_state: SD ≈ 2.5, shared by Senate, Governor, and House races in the state.
  - ε_race: SD ≈ 2.5–3.5, larger with sparse polls or independents.
  - Total SD ≈ 4.5–5.5 on margin.
- **Systematic-miss mixture:** with p ≈ 0.1, add an extra N shift of ±3–4.
- **House:** generic ballot (D+7.4 to D+8.1) → national House vote (likely-voter adjustment) → district lean + incumbency + candidate effects. District SD ≈ 5–7 plus shared N. Use ~500k draws for the seat distribution.
- **Clusters to encode:**
  - national;
  - "red-state reach" (TX, OH, IA, KS, AK, SC);
  - New England incumbency (ME/Collins);
  - Midwest industrial (MI, OH);
  - House suburban seats.
- **Calibration target:** per-race and chamber probabilities within 2–4 pts of Kalshi/Polymarket mids. Assume a bug before assuming edge.
- **Performance:** 10⁶ draws in seconds, multithreaded. Output joint vectors plus precomputed scenario bundles.

## 6. Tournament simulator (`tourney`)

- **Field of N players by type:**
  - passive (≈1.0×);
  - consensus Kelly (0.8–1.3×);
  - YOLO all-in longshot buyers (fraction f_y; contract chosen with price-weighted probability; ends at 100k/p_j or 0);
  - bots with election-night compounding.
- For each world ω: T(ω) = max multiple in the field. P(1st) = E[1{M_you > T}], with ties split.
- **Prior thresholds (low confidence; replace with observed data):** N≈500 → T~8–15×; N≈2,000 → ~15–30×; N≈10,000 → ~25–100×, capped by the cheapest winning price.
- **Honest expectation:** P(1st) ≈ 1–5% at N≈2,000; top-3 about 2–3× that.

## 7. Tests

- **Unit/property:**
  - OMS transitions and no double-send;
  - risk rejects any order breaching caps (proptest);
  - sim moments match the parameters;
  - needle recovers the truth on synthetic counties;
  - tourney matches analytic cases.
- **Paper:** local matching simulator (price-time priority assumption, to be confirmed Oct 1).
- **Replays:**
  - CA SoS test-day captures;
  - synthetic 2022 and 2024 election nights (final county results plus realistic batch ordering by vote type);
  - chaos test (feeds and network killed mid-replay).
- **Dress rehearsals:** two full runs by Nov 2.

## 8. Timeline

- **By Sept 30:**
  - register, opt in to recruiting contact, and read the Terms of Service;
  - create the API key and read the API docs;
  - gateway/OMS/risk plus journal (paper);
  - collectors and fair value;
  - sniper and pair checker (paper).
- **Oct 1, first hour:** `measure_day1` and the decision table.
- **Oct 1–10:** first trade; MM or sniping; Info-tab parsing; `sim` v1.
- **Oct 11–25:** `needle` v1; SoS adapters; CA test captures; replays; `tourney`; draft scenario book.
- **Oct 26–Nov 2:** scenario tranches; rehearsals ×2; chaos test; closing-time table.
- **Nov 3 (ET):**
  - 5pm: systems up, dry run.
  - 7pm–midnight: East and Midwest rolling.
  - Midnight–6am: West Coast and Alaska; late-market positioning.
  - 6am–11:45am: slow-count positioning, then freeze. **No trades after 11:55am.** A human monitor with the kill switch is on duty throughout.
- **Nov 4 onward:**
  - archive the journal privately;
  - scrub the repo;
  - make it public (code, methodology, sim, external backtests, aggregate results);
  - send the write-up to SIG recruiting;
  - email predictionscup@sig.com before publishing anything derived from platform data.

## 9. Failure modes

| Failure | Mitigation |
|---|---|
| API outage at peak | Pre-staged orders; graceful degrade; manual website fallback |
| Bad county data | Require 2 sources or 2 batches |
| Sponsor reverses trades | Size glitch trades ≤2% |
| Self-cross | OMS self-match prevention |
| Laptop dependency | The Oracle Always Free server plus watchdog and the Cloudflare cron-Worker dead-man switch |
| Credential leak | Keychain or sops; private repo; gitleaks |
