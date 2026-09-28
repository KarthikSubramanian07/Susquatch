# LANGUAGES_AND_BOUNDARIES — Susquatch

This file is the authority on languages, process boundaries, and the handoff from the Python bridge to Rust. It overrides any conflicting text in PROMPT.md, SPEC, or ITERATION_AND_IMPROVEMENTS.md.

## Principle

Keep Rust and C++, but **connect them only through messages and files between separate programs. Never compile them together.** No C ABI, no FFI, and no Rust↔C++ linking. The only in-process binding allowed is **pybind11 between C++ and Python** (for research and replays).

## Who does what

### Rust: everything that trades live

This is the part that must never crash or double-send an order. It covers:
- the platform connection (`SuperMarket` adapter behind the `PlatformAdapter` trait);
- the gateway and adaptive rate limiter;
- the book;
- the OMS (idempotent keys, reconcile, self-match prevention);
- risk (caps, breaker, locked legs, kill switch);
- all strategies (mm, sniper, constraints racer, scenario executor, election-night executor);
- the **election-night results feeds** (`enr`: CA SoS JSON, Clarity/Scytl, other SoS; dedupe, cross-source checks, batch-type tagging);
- the journal.

### C++20: the heavy modeling, as three independent pieces

1. **`sim`** (correlated race Monte Carlo) and **`tourney`** (competitor field / P(1st) model) are **C++ libraries called only from Python** research code via pybind11.
   - Once a day, or whenever inputs change (new polls, real-money price moves beyond a threshold, fitted leaderboard updates), Python runs them.
   - Python writes a **versioned target file**: `targets/targets-<utc_ts>-<hash>.json`, plus an atomically swapped `targets/current.json` symlink or pointer. Contents:
     - schema version and generation timestamp;
     - the input data hashes;
     - per-market fair probabilities;
     - the scenario-book target positions (with locked-leg flags);
     - cluster exposures;
     - the P(1st) estimate.
   - The Rust `scenario_executor` reads this file. Rust never calls C++.
2. **`needle`** (election-night county expected-vote model) runs as **its own C++ program (service)**.
   - It subscribes to county-result messages that Rust `enr` publishes on the message bus.
   - It publishes updated **race posteriors and the national-shift posterior** back on the bus.
   - The Rust election-night executor subscribes to those posteriors.
   - `needle` also has **pybind11 bindings** so Python can replay past election nights (2022/2024 synthetic replays, CA SoS test captures) through the exact same code.

### Python: research, calibration, collectors, optimizer, replays, reports, LLM agents

Includes Kalshi/Polymarket read-only collectors, polls, calibrating `sim` to market mids, scenario selection, replay harnesses, and daily reports. LLM agents stay off the hot path.

## Message bus

- Use **NATS** (local server on the VM). If NATS is unavailable, fall back to a Unix domain socket with length-prefixed frames. Pick one, document it, and test it under failure.
- **Subjects:**
  - `enr.county_results` (Rust → needle);
  - `needle.race_posteriors` and `needle.national_shift` (needle → Rust);
  - `needle.heartbeat` (needle → Rust, every ≤2 s);
  - `ops.kill` (any → all).

## One shared format

- Define every message and file **once** in a single schema source under `contracts/`. Use **Protocol Buffers** (bus messages) plus **JSON Schema** (target files). Alternatively, use protobuf for both with a JSON mapping. Generate Rust, C++, and Python types from it. No hand-written duplicate structs.
- Each message and file carries `schema_version`, `produced_at` (UTC ns), `producer` (name + git SHA), and a monotonically increasing `seq`.
- **Cross-language golden tests** (in CI): a fixture set of messages and files is serialized by each language and deserialized by every other language, and they must round-trip identically, byte-for-byte where canonical and field-for-field otherwise. Include edge cases: empty books, probabilities at 0 and 1, max-size payloads, unknown future fields.

## Rust refuses stale or malformed inputs

- **Target file:**
  - Reject on schema mismatch, failed validation, hash mismatch, or `produced_at` older than a configured max age (e.g., 36 h in October; tighter in the final days).
  - When rejected, keep the last good targets but **stop increasing exposure**, and flag it on the dashboard and in the daily summary.
- **Needle:**
  - If no heartbeat or posterior arrives for a configured interval (e.g., 10 s on election night), or a `seq` gap or out-of-order message shows up, the election-night executor **stops acting on needle posteriors**: no new orders from them, and resting orders based on them are cancelled.
  - It resumes only after N consecutive healthy messages.
  - It never trades on old numbers.
- **Validation:** probabilities must be in [0,1], ladders must sum within tolerance, race IDs must exist in the current market map, and every value must be finite. Any failure means drop the message, log it, and increment an error counter. The risk breaker trips if error counts spike.

## Process supervision and ops

- **Separate processes under systemd or Docker Compose:** `rust-live` (can be split by role), `needle`, `nats`, `python-scheduler` (collectors plus the daily sim/tourney run), and the watchdog.
- A crash in `needle` or the Python jobs **cannot** take down order handling. Rust keeps running and simply degrades as described above.
- Each process exposes health metrics (Prometheus). The watchdog texts me when anything is unhealthy.

## Python bridge for the first days of October (fixed calendar)

Trading opens **Oct 1, 12:00pm ET** and nothing moves that date. Until the Rust live system passes its tests, a **small Python bridge** covers only:
1. `measure_day1`;
2. the **first qualifying trade** (1 contract, near mid);
3. the **stale-order sniper** and the **constraints racer**, with the same risk caps as the spec, the same journal format, and deterministic idempotency keys.

**Handoff rules:**
- The bridge and the Rust system **never run order-placing strategies at the same time.** Enforce this with a lock file or lease on the VM. Whoever holds the lease trades; the other stays read-only.
- Cut over to Rust once Rust passes:
  - unit, property, and golden tests;
  - ≥24 h of paper running against the live read API;
  - a live smoke test (1 contract placed and cancelled).
- After cutover, the bridge is archived. It is not maintained as a second trading path.
- Target cutover: within the first 1–2 weeks of October. October barely affects P(1st), so there's no need to rush the Rust system at the cost of correctness.

## Tests added by this design

- Cross-language golden round-trip tests for every contract.
- Staleness tests:
  - Rust ignores targets older than the max age;
  - it stops acting within the interval when `needle` goes silent;
  - it resumes only after N healthy messages.
- Malformed-input fuzzing of the Rust deserializers (`cargo fuzz`) and the C++ deserializers (libFuzzer).
- Chaos drill: kill `needle`, NATS, and the Python scheduler mid-replay. Rust must keep order handling correct and stop acting on the missing inputs.
