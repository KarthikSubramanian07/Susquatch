# rust-live: the crates

`rust-live` is the part of Susquatch that touches the exchange. It has one job description:
**never crash, never double-send, never trade on stale inputs.** Everything here is built so
that the answer to "what happens if X fails?" is "it stops adding risk and cancels what X placed",
without anyone being paged.

`rust` · `tokio` · `prediction-markets` · `order-management` · `fail-safe`

## Crate graph

```
                                   ┌──────────────┐
                                   │  susq-live   │  binary: run · paper-run · smoke · cancel-all · lease
                                   │ engine loop  │  status.json · /healthz · /metrics (loopback)
                                   └──────┬───────┘
         ┌──────────────┬─────────────────┼──────────────────┬──────────────┬─────────────┐
         ▼              ▼                 ▼                  ▼              ▼             ▼
  ┌─────────────┐ ┌───────────┐   ┌──────────────┐   ┌─────────────┐ ┌───────────┐ ┌──────────┐
  │ strategies  │ │  targets  │   │   gateway    │   │    bus      │ │   book    │ │   risk   │
  │ pure        │ │ hash +    │   │ THE write    │   │ NATS +      │ │ YES-space │ │ caps +   │
  │ decisions   │ │ staleness │   │ path         │   │ needle      │ │ L2 books  │ │ breakers │
  └──┬───┬──────┘ └─────┬─────┘   └─┬──┬──┬──┬───┘   │ watchdog    │ └─────┬─────┘ └────┬─────┘
     │   │              │           │  │  │  │       └──────┬──────┘       │            │
     │   └──── book ────┼───────────┼──┼──┼──┼──────────────┼──────────────┘            │
     │                  │           │  │  │  └──── risk ────┼───────────────────────────┘
     │                  │           │  │  └─────── oms ─────┼──── write-ahead intents, idempotency,
     │                  │           │  │                    │     reconcile, restart recovery
     │                  │           │  └────── platform ────┼──── PlatformAdapter · SuperMarket (REST)
     │                  │           │                       │     · Realtime (Phoenix/WS) · MockPlatform
     │                  │           └───────── journal ─────┼──── append-only JSONL, key scrubbed
     ▼                  ▼                                   ▼
  ┌──────────────────────────────────────────────────────────────────────────────────────────┐
  │ susq-core: Pips (1e-4, tick 50) · Side/Action · ids · Qty · Clock · Calendar · Config    │
  │ susq-contracts: generated susq.v1 types (never hand-written)                            │
  └──────────────────────────────────────────────────────────────────────────────────────────┘
```

| Crate | What it owns |
|---|---|
| [`core`](core) | Checked `Pips` prices (only valid, on-tick prices can exist), YES-space mapping, id newtypes, `Clock` (real and manual), the contest calendar and time gate, `config/susq.toml` loading, `MarketIndex`. |
| [`journal`](journal) | `journal/YYYY-MM-DD/rust-live.journal.jsonl`: one compact proto3-JSON `JournalEntry` per line, UTC-day rotation, written by a dedicated thread behind a bounded channel, secrets scrubbed. |
| [`platform`](platform) | `PlatformAdapter` (exactly the endpoints in `docs/API.md`, one attempt per call), the `SuperMarket` REST client, typed `ApiError` with `retry_class()`, tolerant wire models, the Supabase Realtime client, and `MockPlatform`, a real in-memory matching engine with fault injection. |
| [`book`](book) | YES-space L2 books with `observed_at` stamps and realtime dirty marks; depth sweeps in the order's own side terms. |
| [`oms`](oms) | Order state machine, deterministic idempotency keys (cross-language test vector), fsynced write-ahead intents, reconcile, restart recovery, positions. |
| [`risk`](risk) | Pre-trade caps (order, market, cluster, gross, glitch), locked legs, self-cross guard, tournament and time gates; loss-per-hour, reject-rate and bus-error breakers. |
| [`gateway`](gateway) | The only write path: kill, lease, mode, freeze, time gate, risk, write-ahead, journal, send with same-key retries. The AIMD rate limiter with a reserved write budget, and the paper broker. |
| [`targets`](targets) | Reads `targets/current.json`, verifies schema, pointer and content hashes, validity and age, and fails safe to "no new exposure". Also reads the live fair feed `state/fair_live.json` (header + fair, rewritten about every 15 s by the research scheduler) for the sniper and market maker. |
| [`bus`](bus) | NATS client (`enr.county_results`, `needle.*`, `ops.kill`), message validation, and the needle watchdog. |
| [`strategies`](strategies) | Constraints racer, sniper, market maker, scenario executor, election-night executor. Each is a pure function from a snapshot to intents. |
| [`live`](live) | The `susq-live` binary and the engine loop that ties it all together. With `--election` it also runs the `enr` pollers and publishes their batches on `enr.county_results`. |
| [`contracts`](contracts) | Generated `susq.v1` types (owned by the contracts branch). |
| [`enr`](enr) | Election-night results pollers (owned by the election-night branch). |

## One order's journey

```
strategy.decide(snapshot) ─► OrderBundle
   │
   ▼  gateway.submit
 kill? ─► lease held? ─► live armed? ─► frozen? ─► time gate ─► risk.check_bundle
   │                                                                  │ all pass
   ▼                                                                  ▼
 oms.prepare: expiration ≤ 16:55Z, key = uuid5(ns, intent_id|payload), PENDING fsynced
   │
   ▼  task per request
 gates again ─► oms.mark_sent (SENT fsynced) ─► POST /orders (journaled)
   │                        ▲
   │ 429 / 409 in-flight / 502 / 503 / network: UNKNOWN, back off, same bytes, same key
   ▼
 ack ─► ACKED / FILLED (fills applied)       terminal 4xx ─► REJECTED (reject breaker)
 retries exhausted / 500 ─► UNKNOWN ─► reconcile resends the same key or cancels the orphan
```

## Safety invariants (and where they are tested)

| # | Invariant | Enforced in | Tested by |
|---|---|---|---|
| 1 | **Paper by default.** Live needs `SUSQ_MODE=live` *and* `ops/GO_LIVE` containing "go live". Removing the file mid-run blocks further live writes. | `gateway::mode`, `Gateway::write_gate` | `mode` unit tests, `gateway` test `live_mode_needs_go_live_and_time_gate_applies` |
| 2 | **Kill beats everything.** `ops/KILL`, the in-process flag and NATS `ops.kill` are checked before every write and before every retry. A kill triggers exactly one cancel-all. Cancels are never blocked by a kill. | `gateway::kill`, `Engine::handle_kill` | `kill_file_blocks_writes_but_not_cancel_all`, `kill_during_retries_stops_them`, session `kill_file_mid_run_cancels_everything_and_freezes` |
| 3 | **Single trader.** Only the holder of an unexpired `TraderLease` places or cancels. It uses flock on a sidecar, atomic tmp+rename writes, TTL and renew, and an epoch that increments on handover. The format is shared with the Python bridge. | `gateway::lease` | lease unit tests, session `a_lease_held_by_the_bridge_prevents_any_write` |
| 4 | **No double-send.** The key is deterministic from `intent_id + payload`. An intent id is refused for 48 h after it is prepared. Retries rebuild byte-identical requests. Nothing is resent after a restart; untracked resting orders are cancelled instead. | `oms` | `no_double_send` proptest (random decisions, outcomes, reconciles and restarts), gateway fault-injection tests |
| 5 | **Write-ahead.** An intent is fsynced (`PENDING`, then `SENT`) before its request is on the wire. | `oms::store`, `Gateway::send` | `store` tests, `restart_never_resends_and_resolves_from_rest` |
| 6 | **Retry only what the docs call safe:** 429 (honouring Retry-After), 409 `REQUEST_IN_FLIGHT` (≥ 90 s), 502 `ORDER_STATUS_UNKNOWN`, 503, and network errors. Every other 4xx is terminal. A 500 goes through reconcile. | `platform::error`, `gateway::retry` | `error` tests, `retries_documented_codes_with_the_same_key_and_never_double_places`, `request_in_flight_waits_at_least_90_seconds` |
| 7 | **Hard time gate.** No order at or after 16:55Z on Nov 4. Every order expires by then (GTD). A one-shot cancel-all fires at the gate. | `core::calendar`, `risk`, `oms::prepare`, `Engine` | `calendar` tests, session `everything_is_cancelled_at_the_last_order_gate` |
| 8 | **No accepted order breaches a cap.** Exposure counts every live order as filled. Reducing risk is always allowed, except on locked legs. | `risk::limits` | `accepted_bundles_respect_every_cap` proptest (2 000 cases) |
| 9 | **Stale targets stop new exposure.** Targets failing any schema, hash, age or validity check keep the last good file but block increases. | `targets` | targets tests and proptest, session `stale_targets_stop_new_exposure_until_fresh_ones_arrive` |
| 9b | **Stale fair values are never traded on.** A fair quote older than `[fair].stale_after_s` (judged from the file's `produced_at_ns`) makes the sniper and market maker cancel what they built on it and stop. A malformed fair file is refused; the last good values keep ageing. Both strategies also block a market whose book is more than `mapping_error_pts` from fair. | `targets::fair`, `Engine::refresh_fair`, `sniper`, `market_maker` | fair reader tests, session `fresh_fair_feeds_the_sniper_and_a_stale_feed_stops_it`, `stale_fair_cancels_market_maker_quotes`, `a_malformed_fair_file_is_refused_and_the_old_values_age_out` |
| 10 | **A silent or garbled needle stops election-night trading.** Silence, a seq gap, out-of-order or stale data mark it unhealthy. Needle-derived orders are cancelled, and trading resumes only after N consecutive healthy messages. | `bus::watchdog`, `bus::monitor`, `Engine::handle_needle`, `en_executor` | watchdog proptest, NATS integration tests (skipped without `nats-server`) |
| 11 | **Only valid prices exist.** A `Pips` value is always on tick and inside [0.005, 0.995], and the JSON we send equals the exact decimal. | `core::price` | price proptests |
| 12 | **The API key never leaves the process.** It lives in the Authorization header only, has a redacted `Debug`, and is scrubbed from the journal. | `core::secret`, `journal::scrub`, `SuperMarket` | `never_writes_the_api_key`, `places_an_order_with_auth_and_exact_body` |
| 13 | **No process trusts its own memory after a restart.** Open orders and positions are read from REST before acting. | `Engine::bootstrap`, `Oms::open` | `restart_never_resends_and_resolves_from_rest` |

## Running it

```bash
cargo test --workspace                       # everything (NATS tests skip without nats-server)
cargo run -p susq-live -- lease              # show the trader lease (no API key needed)
cargo run -p susq-live -- paper-run          # trade on paper against live books (needs SUSQ_API_KEY)
cargo run -p susq-live -- run --election     # election night: also poll results sites → NATS enr.county_results
cargo run -p susq-live -- smoke --exchange 381   # place + cancel one contract through every gate
cargo run -p susq-live -- cancel-all         # needs the lease
```

Environment: `SUSQ_HOME`, `SUSQ_API_KEY` (secret), `SUSQ_MODE`, and optional overrides `SUSQ_BASE_URL`,
`SUSQ_NATS_URL`, `SUSQ_METRICS_ADDR`, `SUSQ_GIT_SHA`, `RUST_LOG`, `SUSQ_LOG_JSON`.
