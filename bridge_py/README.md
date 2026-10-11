# `susq-bridge` — the Oct 1 bridge

> Rust is the long-term trader. This is the small, paranoid Python process that trades from the first
> minute of the Cup until Rust has earned the lease. It does a few things, it does them carefully, and
> then it gets archived.

`python` · `asyncio` · `httpx` · `supabase-realtime` · `idempotency` · `risk` · `paper-first`

What it runs:

| Piece | What it does | Where |
|---|---|---|
| `measure-day1` | Probes the live platform, writes `reports/DAY1.md` + `reports/day1.json`, applies the SPEC §4 decision table | `measure_day1.py` |
| `first-trade` | The qualifying trade: exactly **1 YES share** near the mid of the most liquid market | `first_trade.py` |
| constraints racer | Trades engine-reported relationship violations as hedged bundles, **but only after proving a guaranteed payoff** | `strategies/racer.py` |
| stale-order sniper | Takes resting orders ≥ `min_edge_pts` through a **fresh, sane, human-verified** fair value | `strategies/sniper.py` |
| far quoter | Rests cheap ladders far from fair on near-certain outcomes | `strategies/farquote.py` |
| scenario executor | Buys the research scenario book (`targets/current.json`) in paced tranches under each leg's price cap; port of rust-live's executor (EXP-008) | `strategies/scenario.py` |
| recorder | Cup prices, leaderboard and market lists to Parquet/JSON for research | `recorder.py` |
| reconcile + status | Rebuilds its view from REST every 15 s and on every realtime resync; writes `state/status.json` | `oms.py`, `status.py` |

## Oct 1, step by step

Trading opens **2026-10-01 16:00Z (12:00 ET)**. The bridge can be started any time before.

```bash
# 0. once: the key (read + trade) goes in .env, never in chat or git
echo 'SUSQ_API_KEY=...' >> .env

# 1. the day before: build the market list and let a human verify state/market_map.json
uv run susq-bridge record

# 2. arm live trading (both switches, deliberately):
echo "go live" > ops/GO_LIVE
export SUSQ_MODE=live

# 3. one command for the whole day: waits for the key and 16:00Z, then
#    measure-day1 -> first-trade -> run (racer + sniper + recorder + reconcile + status)
uv run susq-bridge open
```

Without step 2 everything above runs in **paper**: reads hit the real platform, writes go to the paper
broker, and nothing reaches the exchange. That's the default, on purpose.

## Commands

| Command | What it does | Needs |
|---|---|---|
| `susq-bridge open` | Polls for `SUSQ_API_KEY` (every 30 s), sleeps until `calendar.open_utc`, then `measure-day1` → `first-trade` (live only) → `run` | key |
| `susq-bridge measure-day1` | All probes; write probes only in live mode | key (else a report of skips) |
| `susq-bridge first-trade` | 1 share; pays the ask if the spread ≤ 2 ticks, else posts the mid for 60 s then pays the ask; records fees and valuation into DAY1 | live + lease |
| `susq-bridge run [--no-racer] [--no-sniper] [--no-scenario] [--no-recorder] [--no-realtime]` | The long-running process | key |
| `susq-bridge record [--loop]` | Recorder only (markets + node trees, prices, leaderboard) | key |
| `susq-bridge cancel-all [--exchange ID]` | Tournament-scoped cancel-all, confirmed against `GET /orders?status=open` | lease |
| `susq-bridge status [--refresh]` | Prints `state/status.json` (health, ACTION NEEDED) | — |
| `susq-bridge lease show` / `lease release [--force]` | Inspect or expire the trader lease (refuses while the holder is alive) | — |

All commands take `--home` (default: the current directory, or `$SUSQ_HOME`). Config comes from
`config/susq.toml`, then `.env`, then the environment (`SUSQ_API_KEY`, `SUSQ_BASE_URL`,
`SUSQ_TOURNAMENT_SLUG`, `SUSQ_MODE`).

## The safety model

There is no pager. The bridge has to fail safe on its own, so every write goes through **one** path,
`gateway.py`, and that path runs the same gauntlet every time:

1. **Kill beats everything.** `ops/KILL` exists → cancel-all (once), freeze, refuse. Checked before
   every write and every second in the background. Stays frozen until restart.
2. **Lease.** Only the holder of `ops/state/trader.lease` places or cancels. `flock` + atomic
   replace, 30 s TTL renewed every 5 s, takeover only after expiry, `epoch` bumps on every new holder.
   Rust and the bridge can never both trade.
3. **Breakers.** More than 10 rejects a minute, or a 3 % mark-to-market loss inside an hour → freeze and
   cancel-all.
4. **Mode.** Paper unless `SUSQ_MODE=live` **and** `ops/GO_LIVE` says "go live".
5. **Time gate.** No new order at or after `calendar.last_order_utc` (**16:55Z Nov 4**), a cancel-all
   fires at that instant, and **every order carries `expirationDate = min(now + ttl, 16:55Z)`**. A
   crashed bridge's orders die on their own.
6. **Risk.** Per-market 5 %, per-cluster (national 25 / region 15 / state 10 / House 20 %), gross
   60 %, per-order 2 % of equity; glitch trades (> 15 pts through fair) share a 2 % budget; locked
   legs can't be reduced; we never send an order that crosses our own resting order; `tournamentId`
   must be ours. Caps shrink an order instead of rejecting it where they can; a hypothesis property
   test checks that **no accepted order ever breaches a cap**. Once a reconcile has reported cash, a
   **buying-power** cap also applies: what the order consumes must fit in cash minus what our resting
   orders could consume. By default that is the full notional of the shares it opens (see
   [ALL collateral](#all-collateral) for the opt-in alternative).
7. **Write-ahead intent.** The `OrderIntent` (with its deterministic idempotency key,
   `uuid5(namespace, intent_id | canonical_payload)`) is fsynced to `state/bridge/intents.jsonl`
   **before** the request leaves.
8. **Retries only where the docs say it's safe**: 429 (honouring `Retry-After`, and the limiter halves
   its rate), 409 `REQUEST_IN_FLIGHT` (≥ 90 s), 502 `ORDER_STATUS_UNKNOWN`, 503, 5xx and network errors.
   Always the same key, exponential backoff with jitter, never past the order's own expiry. Every
   other 4xx is terminal.
9. **Journal.** Every request, response, error and decision lands in
   `journal/YYYY-MM-DD/bridge.journal.jsonl` as `susq.v1.JournalEntry` lines. The API key and realtime
   tokens are redacted before they touch disk.

And after a restart the bridge trusts nothing it remembers: it reconciles open orders, positions and
equity from REST, re-sends fresh unresolved intents **with their original key** (the platform replays
instead of double-placing), and closes out stale ones.

## The racer's proof

For each violation (pushed on `relationships:violations:{tournamentId}`, polled every 3 s only while
realtime is down) the racer builds a bundle and evaluates its payoff in **every joint outcome the
relationship allows**. It trades only if the minimum payoff beats the worst-price cost by `min_edge`.

- Mutually exclusive, Σ YES bids > 1 → buy NO on every leg. Safe even with an independent.
- Mutually exclusive, Σ YES asks < 1 → buy YES on every leg, and **only** if the engine says the set is
  exhaustive *and* every leg has a human-verified rule with `independent_can_win = false`. Nebraska
  (an independent can win, both party markets can resolve NO) fails the proof and is refused.
- Implication A ⇒ C with P(A) > P(C) → C YES + A NO: pays at least 1 in (1,1), (0,1) and (0,0).

Legs are sized by walking the book, capped at 1 % of equity per bundle, sent atomically through
`/orders/multi-leg` (never with `relationshipConstraint`, which would reject arbitrage), and any unfilled
remainder is cancelled at once. Latency from violation to acknowledgement is journaled.

## ALL collateral

The platform backs part of a relationship-linked fill with the component's **guaranteed payout
floor** instead of cash (NO on both legs of a pair pays at least 1, so the second leg can cost almost
nothing). The engine's exact rules are not documented, so the bridge keeps two things apart:

| | What happens | When |
|---|---|---|
| **Record** | every order response's `all` block is journaled as `all_economics`, next to what the shared model (`susq.platform.collateral`, the same code as the mock and paper broker) predicted for that fill; counters and the last known outstanding advance per component live on `gateway.collateral` | always |
| **Size** | the buying-power cap and the racer's per-bundle budget count the modeled *effective* cost (`netBuyingPowerImpact`-style: nominal minus the advance) instead of full notional; bundle legs are costed together, in order | with `[risk] use_collateral_buying_power = true`, or `"auto"` once day 1 confirmed the model |

The flag ships **`"auto"`**: off until `measure-day1` confirms the model, then on by itself (recorded in
`state/bridge/collateral_confirmed.json`, so a restart keeps it; a later measured mismatch removes it).
`true` / `false` force it either way. `measure-day1` decides: its `collateral` section compares the engine's `all`
on a linked fill (the racer's, or live-only a 1-share NO+NO probe on a mutually exclusive pair, which
pays at least 1 whatever happens) with the model field by field, and the decision table says
"set use_collateral_buying_power = true" only if every field matches within 0.01 SUSQies. Otherwise
it names the mismatch and keeps the flag off. Exposure caps (per market, cluster, gross) stay on full
notional either way. Paper mode applies the same model to paper fills once the racer has loaded the
relationships.

## The sniper's gates

Only markets whose `MarketRule` in `state/market_map.json` is `verified: true`; only with a fair value
from `state/fair_live.json` younger than 60 s; never when the Cup mid is more than 25 points from fair
(that's how an inverted D/R mapping looks, so the market is blocked and flagged in the status). It
takes exactly the stale levels, sized so that a completely wrong fair loses under 1 % of bankroll.
Orders whose fair input goes stale are cancelled by the reconcile loop.

## The scenario executor (EXP-008)

Reads `[strategies.scenario]` (the same section rust-live reads) and the last good target file. Each pass
(every 30 s) it sends at most one buy per target leg: a quarter of the gap to target, never through zero,
priced at the ask when that is within the leg's YES-space `max_entry_price` (NO buys: `1 - max_entry_price`)
and resting at the cap otherwise. It waits for `start_utc`, keeps `min_interval_s` between tranches on one
exchange, pauses once the trailing hour's tranches exceed `max_hourly_notional_frac` of equity, and clamps
each tranche to `[risk] max_order_notional_frac` of equity. It only trades verified markets on a fresh book,
never reduces a locked leg (those also go to the risk engine, which then refuses any strategy's order that
would reduce one), and while the target file is stale or invalid it only reduces. Decisions are journaled as
`scenario_pass` (when the notes change) and `scenario_order`. `tests/fixtures/scenario_tranches.json`
holds it and rust-live to the same tranches.

## Files

| Path | What |
|---|---|
| `journal/YYYY-MM-DD/bridge.journal.jsonl` | every platform exchange + decision (Competition Data) |
| `state/bridge/intents.jsonl` | write-ahead intents, last line per `intent_id` wins |
| `state/market_map.json` | input: verified market rules (`susq.v1.MarketMap`) |
| `state/fair_live.json` | input: `TargetFile` with `header` + `fair`, rewritten every ~15 s |
| `targets/current.json` | input: pointer to the research target file whose `positions` are the scenario book |
| `state/status.json` | output: `StatusSnapshot` every minute |
| `data/cup/{prices,leaderboard}/date=…/part-….parquet`, `data/cup/markets/` | research data |
| `reports/DAY1.md`, `reports/day1.json`, `reports/first_trade.json` | day-one measurements (incl. the `collateral` verdict) |
| `ops/KILL`, `ops/GO_LIVE`, `ops/state/trader.lease` | the switches and the lease |

Everything under `journal/ state/ data/ reports/` is gitignored and blocked by the pre-commit hook.

## Testing

```bash
uv run pytest -m "not live" bridge_py/tests      # everything, against the mock exchange (~40 s)
uv run pytest -m live bridge_py/tests            # read-only smoke test against the real API (needs the key)
```

The tests drive a full `Bridge` against `susq-mock` (see `py/susq/mock/README.md`), including
502-was-actually-placed retries, a kill mid-run, lease contention between OS processes, the time gate,
restart recovery, paper-vs-live routing, the racer refusing an unsafe exhaustive bundle, the sniper
refusing stale and mis-mapped fair values, a realtime violation raced end to end over a real
websocket, and ALL collateral: `all` journaled with the flag off, the racer sizing bigger only with it
on, and the day-one verdict refusing the flag when the engine returns `all: null`.
