# Runtime layout & process contracts

Everything below lives under `$SUSQ_HOME` (the repo checkout on the server). Paths marked **CD** hold
**Competition Data**. They are gitignored, blocked by the pre-commit hook, and never pushed anywhere public.

| Path | Owner (writer) | Readers | Format | Notes |
|---|---|---|---|---|
| `config/susq.toml` | humans (via PR) | everyone | TOML | non-secret defaults |
| `.env` | human | everyone | dotenv | secrets: `SUSQ_API_KEY`, Cloudflare, … |
| `journal/YYYY-MM-DD/<producer>.journal.jsonl` **CD** | bridge, rust-live | reports | one `susq.v1.JournalEntry` (compact proto3 JSON) per line | every platform request/response; API key never written |
| `state/<producer>/intents.jsonl` **CD** | bridge, rust-live | same process on restart | `susq.v1.OrderIntent` per line, last line per `intent_id` wins | written **before** send |
| `state/market_map.json` **CD** | `/parse-rules` + human | strategies | `susq.v1.MarketMap` JSON | strategies trade only `verified: true` rows |
| `state/status.json` **CD** | current lease holder | status pusher → KV | `susq.v1.StatusSnapshot` JSON | aggregates only |
| `ops/state/trader.lease` | lease holder | bridge, rust-live | `susq.v1.TraderLease` JSON | atomic write (tmp + rename) + `flock` |
| `ops/KILL` | human, dashboard poller, watchdog | everyone | any text (the reason) | presence = cancel-all + freeze |
| `ops/GO_LIVE` | human only | gateways | must contain `go live` | plus `SUSQ_MODE=live` |
| `targets/targets-<utc>-<hash8>.json`, `targets/current.json` | python-scheduler | rust-live | `TargetFile` / `TargetPointer` JSON | atomic pointer swap |
| `data/**.parquet` **CD** (cup), public (kalshi/poly) | collectors | research | Parquet (zstd) | |
| `reports/DAY1.md`, `reports/daily/`, `reports/drills/`, `reports/packets/` **CD** | reports | humans, `/daily-review` | Markdown + CSV | packets are aggregated before going to the private reports repo |

## Process rules

1. **Only the lease holder places or cancels orders.** The check lives in the gateway (Python and Rust), not in strategies.
2. **Kill beats everything.** Gateways check `ops/KILL` before every write. On kill: cancel-all (scoped to the tournament), then freeze.
3. **Paper is the default.** In paper mode the gateway routes writes to the paper broker, which simulates fills against live books.
4. **Every order** is a limit order with `tournamentId` and an `expirationDate ≤ calendar.last_order_utc`. It also needs an idempotency key
   derived as `uuid5(oms.idempotency_namespace, intent_id + "|" + canonical_payload)`, where
   `canonical_payload = "{exchange_id}|{side}|{action}|{quantity}|{price_pips}|{expiration_unix_s}|{tournament_id}"`
   (`side` ∈ yes/no, `action` ∈ buy/sell). Test vector: intent `racer:rel-9:1791475200`, payload
   `381|no|buy|37|1450|1791475230|t-uuid`, default namespace → `c5fe4b7e-7efc-55fb-82b5-1bd6356d699c`.
5. **Retry only what the docs say is safe**: 429, 409 `REQUEST_IN_FLIGHT`, 502 `ORDER_STATUS_UNKNOWN`, 503, and network errors. Always use the same key.
6. **No process trusts its own memory after a restart.** Reconcile open orders and positions from REST first, then act.
