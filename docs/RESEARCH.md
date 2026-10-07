# Research side: how Susquatch decides what the world is worth

> Real money tells us the odds. The simulator tells us how the odds move *together*.
> The field tells us which world is already crowded. The rest is arithmetic.

This is the Python half of Susquatch that never touches an order. It watches Kalshi and
Polymarket, maps every Cup market onto a real-money race, publishes fair values every 15 seconds,
fits a correlated election simulator to the markets once a day, picks the scenario book that
maximizes **P(1st)**, and writes it all down in files that `rust-live` and the bridge consume.
Everything here is read-only with respect to the Cup. The scheduler imports no trading client.

```
            Kalshi (public v2)          Polymarket (Gamma + CLOB)          data/cup/** (bridge recorder, CD)
                   │                              │                          markets · prices · leaderboard
                   └──────────┬───────────────────┘                                 │
                              ▼                                                     │
                   collectors/  every 60 s ──► data/{kalshi,polymarket,reference}/date=…/part-….parquet
                              │                                                     │
                              ▼                                                     ▼
   mapping/  draft map ◄── races.py (one parser for every venue) ──► state/market_map.json (verified=false)
                              │
          ┌───────────────────┼───────────────────────────┬──────────────────────────┐
          ▼                   ▼                           ▼                          ▼
   fair/ every 15 s     calibrate/ daily            telemetry/ hourly           reports/ 07:00 PT
   state/fair_live.json  sim ⇄ real money            leaderboard → fitted T      reports/daily/<date>.md
   (header + fair only)  chamber audit               crowd drift → exposure      reports/packets/<date>/
                              │                           │                     (aggregates, scrubbed)
                              ▼                           │
                        optimizer/ scenario book ◄────────┘
                              │
                              ▼
            targets/targets-<utc>-<hash8>.json  +  targets/current.json  ──► rust-live
```

## Running it

```bash
uv run susq research collect          # one live snapshot of both venues (public endpoints)
uv run susq research map-draft        # draft state/market_map.json from the newest Cup listing
uv run susq research calibrate        # fit the sim, print the chamber audit
uv run susq research fair             # write state/fair_live.json once
uv run susq research telemetry        # leaderboard + crowd drift → state/research/telemetry.json
uv run susq research targets          # calibrate + optimize + write a target file
uv run susq research report --day 2026-10-05
uv run susq research run              # the long-lived python-scheduler
```

`$SUSQ_HOME` (default: the current directory) is the root of the runtime layout in
[`docs/RUNTIME.md`](RUNTIME.md). Tests never touch the network: `uv run pytest -m "not live"`.

| Job | Cadence | Writes |
|---|---|---|
| `collect` | every 60 s (`[telemetry].price_snapshot_interval_s`) | `data/kalshi`, `data/polymarket`, `data/reference` |
| `fair` | every 15 s | `state/fair_live.json`, `state/research/fair_flags.json` |
| `telemetry` | hourly (`[telemetry].leaderboard_interval_s`) | `state/research/telemetry.json` |
| `targets` | 06:30 PT daily | `state/research/{sim_probs,calibration}.json`, `targets/*` |
| `targets-if-changed` | hourly | reruns `targets` if any consensus moved > 3 pts or fitted T moved > 25 % |
| `report` | 07:00 PT daily (previous complete UTC day) | `reports/daily/<date>.md`, `reports/packets/<date>/` |

Each job runs in a worker thread inside its own asyncio task. A crash is logged, counted in
`state/research/scheduler.json` and retried with backoff: never faster than the job's own
interval, and within minutes for daily jobs. One job failing never blocks another.

## Collectors: free, public, polite

| Venue | Discovery | Quotes | Pace |
|---|---|---|---|
| Kalshi `api.elections.kalshi.com/trade-api/v2` | `GET /events?status=open&with_nested_markets=true` (≈60 pages, every 6 h) | `GET /markets?tickers=…` batches of 100 | 4 req/s (Basic tier allows ~20) |
| Polymarket | Gamma `GET /events?tag_slug=midterms` | CLOB `POST /books` batches of 100 | 5 req/s (`/books` allows 50) |

Every request goes through `PoliteClient`: a minimum gap between requests, retries only on
429/5xx/network errors, and `Retry-After` honoured. The two venues fail independently. If the
CLOB is down, Gamma's cached top of book is used and tagged `source=gamma`.

Things the live endpoints taught us on 2026-09-28 (all covered by tests with the recorded fixtures):

* **Tickers lie; titles don't.** `SENATELA-26` is titled *"Kentucky Senate winner?"*, so the race is
  parsed from the title and the disagreement is kept as a warning.
* **Some cycle suffixes are wrong.** `GOVPARTYKS-27` is the Kansas governor race on the 2026
  ballot (override list), while `GOVPARTYNH-28` is ambiguous and skipped. Polymarket covers NH.
* **Stale prints happen.** An Idaho Senate longshot showed a 0.1¢ ask next to a 99.9¢ last trade.
  `blend_mid` trusts a last trade only if it sits inside whatever side of the book exists.
* `POST /books` takes a JSON *list* of `{"token_id"}` (the docs disagree with each other) and
  returns levels best-last, so the code takes max(bids) and min(asks) without trusting order.

### Schemas (Parquet, zstd, `date=YYYY-MM-DD/part-<utc>.parquet`)

| Table | Columns |
|---|---|
| `data/kalshi` | ts, event_ticker, ticker, title, status, yes_bid, yes_ask, last, yes_bid_size, yes_ask_size, volume, volume_24h, open_interest, race_id, party |
| `data/polymarket` | ts, event_slug, market_id, question, token_id, best_bid, best_ask, bid_size, ask_size, last, volume, source (`clob`/`gamma`), race_id, party |
| `data/reference` | ts, venue, race_id, kind, party, venue_market_id, title, bid, ask, last, mid, spread, volume |
| `data/cup/tape` | ts (trade `createdAt`), exchange_id, market_id, trade_id, side (`YES`/`NO`, the taker's space), price, size, volume, recorded_at |

`ts` is `timestamp[us, UTC]`. Cup inputs, written by the bridge recorder, are read from
`data/cup/{markets,prices,leaderboard,tape}` with the schemas documented in `telemetry/cupdata.py`.
The tape is the field's actual positioning per market (which side was bought, at what price, in what
size), which leaderboard marks cannot show; `cupdata.read_tape` returns it oldest first and unique by
trade_id. The bridge records it every `[telemetry].tape_interval_s` (30 min) with at most
`tape_max_requests_per_pass` reads per pass; `data/cup/tape/state.json` holds the per-exchange cursor.

## Mapping: one parser, exact joins, never auto-verified

Every venue's phrasing lands on the same key, so joining is an exact `(race_id, party)` match:

| Race id | Meaning |
|---|---|
| `2026-SEN-NH` | Senate seat |
| `2026-SEN-OH-S` | Senate special (OH and FL are the only 2026 Senate races in those states) |
| `2026-GOV-GA` | Governor |
| `2026-HOUSE-NY-17`, `2026-HOUSE-AK-AL` | House seat (two-digit or at-large) |
| `2026-CONTROL-SENATE`, `2026-CONTROL-HOUSE` | chamber control, `kind = "chamber"` |

Chamber rows carry a race id so fair values and scenarios can join on them. The proto comment
suggests `""` there. That is a deliberate, documented deviation.

Each `MarketRule` gets clusters (`national`, `region:<northeast|midwest|south|west>`, `state:XX`,
`house`, plus `red-state-reach` TX/OH/IA/KS/AK/SC, `new-england-incumbency` ME,
`midwest-industrial` MI/OH), `rcv` (AK; ME for federal races), `runoff_possible` (GA, LA),
`manual_settlement` (chambers), and `independent_can_win`. That last flag is always set for NE
Senate and is also set wherever a venue prices an independent at 1% or more (live: ID, MT). A
complement reference (buying R to price D) is only allowed in two-party races and is flagged as
inverted. `notes` starts with `confidence=0.xx` followed by the evidence. A Cup price more than
`mapping_error_pts` from real money costs 0.5 confidence and is marked `PRICE_DISAGREES`.
**`verified` is always false.** Humans verify through `/parse-rules`. Re-drafting keeps every
verified row of the live map exactly as the human left it, and the live map is only overwritten
on first creation or with `--promote`.

## Fair value

```
fair = w_k · m + w_s · s          (w_k, w_s) = [fair].kalshi_weight, sim_weight = (0.8, 0.2)
```

* **m**, real money, comes from the row's own Kalshi ticker and Polymarket token, inverted when the
  row says so. Venue mids are weighted by 1 / max(spread, 1¢), and quotes older than
  `quote_max_age_s` (150 s, about 2.5 collect cycles) are ignored. Consumers separately apply
  `[fair].stale_after_s` to the fair file's `produced_at_ns`.
* **s**, the sim, is the calibrated probability for (race, party) from `sim_probs.json`.
* If only one source exists it is used alone, and `source` says which (`market(kalshi+polymarket)`,
  `sim`, `0.8*market(kalshi)+0.2*sim`).
* **80% band**, in logit space: σ_m = max(half-spread, venue disagreement / 2, ½¢),
  σ_s = max(|s − m|, 2¢), and σ_logit = √((w_k σ_m)² + (w_s σ_s)²) / (p(1−p)), giving
  band = logistic(logit p ± 1.2816 σ_logit). A sim-only fair value gets σ = 8¢, so it is loudly
  uncertain.

Because the sim is calibrated to the same markets once a day, for single races it acts as a slow
anchor: today's live mid pulled 20% toward this morning's. It carries real information where
real money is thin or absent: chamber control implied by race outcomes, and unpriced races.

## The simulator (SPEC §5, `models/numpy_engine.py`)

For race *i* with left-side mean μᵢ (points of two-way margin, left = Democrat, or the
independent where one carries the anti-R vote):

```
marginᵢ = μᵢ + βᵢ·N + R_region(i) + S_state(i) + Σ_c C_c·1{i ∈ c} + εᵢ

N   = 2.75 · t₅ · √(3/5)  +  miss,      miss = ±3.5 with probability 0.10
R   ~ 𝒩(0, 1.5²) per region       S ~ 𝒩(0, 2.5²) per state (shared by SEN/GOV/HOUSE)
C_c ~ 𝒩(0, 1.5²) per named cluster  εᵢ ~ 𝒩(0, 3²)   (4² when an independent is the left side)
```

The unconditional margin SD for a plain state race is about 5.1 points, inside SPEC's 4.5–5.5 band.
Tests check the moments against `analytic_margin_sd` and check the kurtosis of N. They also check
that correlations order correctly: same state > shared named cluster > same region > national only.

**Calibration** draws the shocks once with μ = 0 and bisects each μᵢ until
mean(shockᵢ + μᵢ > 0) = pᵢ, where pᵢ = left / (left + R) from the real-money consensus. Common
random numbers mean a price move changes μ and never the correlation structure. Unpriced Senate
seats default to 97% for the incumbent party; other unpriced races are left out.

**Chambers.** The Senate is counted: 34 D and 31 R seats are not up, 35 are simulated, and
Democrats need 51 (a 50–50 split goes to the Republican VP). An independent winner caucuses with
Democrats on a per-draw coin flip, since Osborn says he will caucus with neither. That assumption
is explicit and cheap to change. **The House is modelled as a single calibrated race-like
contract** (β = 1 on N, the shared `house` cluster, ε = 3). No district-lean data is available
offline, and a fake seat model would be worse than an honest pseudo-race. Its gap to the market is
zero by construction.

**The audit that matters:** Senate control *implied by the race outcomes* versus the Senate
control market. SPEC wants 2–4 points; more than 4 points is reported as *"assume a bug before
assuming edge"* and shows up first in the daily report. Live runs on 2026-09-28, using the SPEC
parameters as they stand, gave **59.5% and 59.2% implied vs 62% market (−2.5 / −2.8 pts)**. The
naive independent product in RESEARCH §5 was 64%, so the correlation matters in the direction
you would expect.

`models/facade.py` prefers the C++ `susq_models` extension when it exposes

```
SIM_API_VERSION = 1
simulate_margins(mu, beta, race_sd, region_idx, state_idx, membership, cluster_sd,
                 n_regions, n_states, nu, national_sd, region_sd, state_sd,
                 miss_p, miss_shift, n_draws, seed) -> (margins[n,k], national[n], clusters[n,c])
TOURNEY_API_VERSION = 1
simulate_threshold(outcomes[n,j] uint8, prices[j], n_players, frac_kelly, frac_yolo, frac_bot,
                   kelly_lo, kelly_hi, yolo_price_power, bot_log_mean, bot_log_sd, seed) -> T[n]
```

with numpy's exact semantics (index −1 = no region/state). Anything else falls back to numpy, and
the engine name is written into every target file's notes.

## Tournament model and the winning multiple T (SPEC §6, ITERATION §2.2)

**Field model.** Passive players finish at 1×. Consensus-Kelly players draw from U(0.8, 1.3).
Bots are lognormal. YOLO players go all in on one contract, chosen with probability ∝ price,
and finish at 1/p if it resolves YES and 0 otherwise. T(ω) is the field maximum. Maxima of iid
draws use F⁻¹(U^{1/n}), so N = 10,000 costs the same as N = 100. P(1st) = E[1{M > T} + ½·1{M = T}].
Tests cover the analytic cases: a passive field gives T = 1, and a single contract at 10¢ gives
T = 10 exactly when it hits.

**Fitted from the leaderboard.** Balance ≈ 100,000 + pnl. We track field size, top-k multiples,
the IQR of log multiples and jumps: a player's multiple rising ≥ 1.5× between snapshots, reported
by rank, never by identity. Marks are not resolved payoffs, so the fit is a documented blend in
log space:

```
log T₅₀ = (1 − w)·log prior₅₀(N) + w·log max(top₁, top₁·e^{g·days_to_lock}),   w = min(1, days observed / 14)
T₅₀ = min(T₅₀, prior₉₀(N))
```

g is the 7-day least-squares growth rate of the **third-highest** multiple, not the top one, and
T₅₀ is capped at the prior's p90 for the observed N (EXP-001). The Cup's final rank is the balance
after every market resolves, but the leaderboard shows marks: the day-one leaders bought
near-certain-to-lose contracts at 0.95 in thin books, so their 10× marks are an artifact of the
mark and will resolve to 0. Fitting g on the top-1 and extrapolating it to lock put T₅₀ at about
93× and made every scenario score P(M > T | S) = 0. The p90/p50 spread still comes from the SPEC
prior (N ≈ 500 → 8–15×, 2,000 → 15–30×, 10,000 → 25–100×, read as p50–p90). The optimizer uses
the fitted T when telemetry exists and the field model otherwise.

## Crowd-position inference (ITERATION §2.3)

For each mapped Cup market, the hourly mean Cup mid is compared with the hourly real-money
reference for the same (race, party), in the **Democratic direction**:
drift = (cup − ref) × 100 × (+1 for DEM/IND legs, −1 for REP legs). A market's drift is
*persistent* when it has ≥ 12 hourly bins, |t| ≥ 3 and ≥ 75% same-sign bins. The thresholds
are strict because hourly bins are autocorrelated. Cluster drift is the mean across the markets
carrying the tag.

Field exposure to a scenario with shock signs sₖ (+1 = D-favourable) is
`e_S = tanh(Σₖ wₖ·sₖ·driftₖ / 3 pts)`, with weight 1 for `national` and ½ for named clusters.
No data gives e = 0.

## The scenario book (SPEC §3 Phase 2)

1. **Grid.** National-shift decile × the sign of every named-cluster shock: up to 10 × 2⁴ = 160
   cells, each a set of simulated worlds. Every world is in exactly one cell.
2. **Consistent legs.** Legs with P(YES | S) ≥ 0.30 that are likelier in S than overall
   (lift ≥ 1.1), ranked by payoff per SUSQie in S, P(YES | S) / ask.
3. **Concentration.** For k = 1…12, spend `book_frac` (60%) of equity on the top-k legs in equal
   cost slices, capped at 12% of equity per leg and by book depth when known. Keep the k with the
   best P(M_S > T_S | S), where M = (cash left + shares that pay) / 100,000. Fewer, cheaper legs
   are the only way a pre-election book reaches the 10–30× that wins.
4. **Crowding.** T_S = T·(1 + 0.5·e_S). A crowded world has more competitors finishing high.
5. **Score.** P(S) · P(M_S > T_S | S). When no book beats T anywhere, which is the normal state
   while tails are expensive, scenarios are ranked by P(S)·E[min(M/T, 1) | S], i.e. how close to
   winning, so the choice is never alphabetical.
6. **Construction.** Quantities are nudged off multiples of 5, and one mid-priced
   **idiosyncratic leg** (the one least tied to S) is added: everyone starts at exactly 100,000,
   so a tie is a real risk. Core legs are **locked** unless their state reports early (poll close
   ≤ 8 pm ET), in which case they double as election-night rolling fuel. Tranches are T1 (best
   half by value), T2 and T3 (the idiosyncratic leg). `max_entry_price` = min(ask + 2 ticks,
   max(fair_hi, ask)), rounded down to the 0.005 tick, and every leg carries a one-line rationale.

Only **verified** map rows are tradable (`--allow-unverified` exists for research runs).
`p_top3` is written as 2.5 × P(1st), SPEC §6's rule of thumb, and the target file's notes say so.

An honest note from the first end-to-end runs: against a field-model T of about 15× (N = 2,000),
a book built from a 27-market listing whose cheapest legs cost 5–15¢ tops out around 3×, so
P(1st) is ≈ 0. That is not a bug. It is SPEC §6 in numbers. It says the book must be built
from the Cup's cheapest tails once the full listing is recorded, and that election-night rolling
remains the other road to first place.

## Target files

`targets/targets-<YYYYMMDDTHHMMSSZ>-<hash8>.json` is a `susq.v1.TargetFile`:
header, tournament slug, `valid_until_ns = produced + [targets].max_age_h`, input hashes
(reference snapshot, market map, latest Cup prices, telemetry, sim parameters), fair values,
positions, cluster limits from `[risk.clusters]`, the tourney estimate and a notes line. The
writer stamps `content_sha256` with `susq.contracts.with_content_hash`, writes atomically, reads
the file back, and re-validates everything: schema version, hash, probabilities in [0,1] with
p_lo ≤ p ≤ p_hi, finite tick-valid prices, non-zero quantities. Only then does it swap
`targets/current.json`. A file that fails is deleted and never pointed to.

## Reports and review packets

`reports/daily/<date>.md` (local, Competition Data) covers P&L by strategy as the mark-to-fair
of the day's fills, fills, constraints-racer hit rate and response latency, Sponsor spread
stats, rank, fitted T, P(1st), risk events, and the **top 3 proposed changes**. Nine
deterministic rules, keyed to the SPEC §4 decision table and the calibration contract, produce
them; the same metrics always give the same three.

`reports/packets/<date>/` (`summary.md`, `metrics.csv`, `strategies.csv`) is the only thing that
may go to the private reports repo. It is built from numbers and fixed enums only. Before it is
published, `scrub()` rejects raw field names (`orderId`, `profileId`, `exchangeId`, …), UUIDs,
numbers with nine or more digits, and any leaderboard username. A leaking packet is deleted.

## Known limits

* The House is one calibrated pseudo-race, not 435 districts (no offline lean data).
* The Senate independent-caucus coin flip, the named-cluster SDs (1.5) and the T-fit blend are
  judgement calls. Each is one constant, and each is logged in the target notes or in this file.
* Cup book depth is not in the recorded price schema, so leg sizes are capped by equity rather
  than by depth until the recorder adds it.
* Poll-close times are approximate (RESEARCH §7) and only decide which legs are rolling fuel.
