# The Super Market API: what Susquatch actually uses

> Source of truth: the **official** reference at <https://sig.thesuper.market/api/v1/docs>
> (OpenAPI 3.1, `info.version = 1.0.0`, fetched 2026-09-28 by `scripts/fetch_api_docs.py`).
> This page is our own condensed reading of it, scoped to a **participant** key. It is not a copy.
> Where this page and the live docs disagree, the live docs win, and we log the change in
> [`brief/ERRATA.md`](../brief/ERRATA.md).

## Hosts & auth

| | |
|---|---|
| Production base URL (per `servers[0]`) | `https://www.thesuper.market/api/v1` |
| Docs host (also serves `/api/v1`, identical 401 shape) | `https://sig.thesuper.market/api/v1` |
| Auth | `Authorization: Bearer <key>` **or** `X-API-Key: <key>` |
| Scopes we need | `read` (all GETs) + `trade` (POST/DELETE orders). Never `admin`. |
| Key binding | SIG participant keys are **organization-bound**. Omitting `tournamentId` silently resolves to the org's *default* tournament. **We always pass `tournamentId` explicitly.** |

Probed unauthenticated on 2026-09-28: both hosts answer `401 {"error":{"code":"MISSING_API_KEY",…}}`,
served by Vercel, with an `x-request-id` header and no rate-limit headers before auth.

## Error envelope

```json
{ "error": { "code": "RATE_LIMITED", "message": "…", "details": { } } }
```

Branch on `code`, never on `message`. The retry classes we implement:

| Class | Codes | Behaviour |
|---|---|---|
| **Retry, same idempotency key** | `429 RATE_LIMITED`, `409 REQUEST_IN_FLIGHT` (wait ≥90 s), `502 ORDER_STATUS_UNKNOWN`, `503 TX_CONFLICT`, `503 SERVICE_UNAVAILABLE` | Exponential backoff with jitter. Honour `Retry-After`. **Never mint a new key.** |
| **Terminal** | every other 4xx (`VALIDATION_ERROR`, `INSUFFICIENT_SCOPES`, `FORBIDDEN`, `TERMS_NOT_ACKNOWLEDGED`, `CONFLICT`, `RELATIONSHIP_VIOLATION`, …) | Surface to the OMS and journal it. `TERMS_NOT_ACKNOWLEDGED` shows up as ACTION NEEDED in the daily summary. |
| **Unknown 5xx** | `500 INTERNAL_ERROR`, `DATABASE_ERROR` | For reads, retry with backoff. For writes, reconcile first (see OMS), then retry with the same key. |

## Prices, ticks, sides

- Prices are **YES-normalized decimals in [0, 1]** everywhere: books, trades, fills, and the realtime feed.
- **Tick = 0.005.** Limit prices must lie on the tick in **[0.005, 0.995]**. Off-tick prices return `400 VALIDATION_ERROR`.
  (The unofficial notes guessed 0.01. They were wrong.)
- A market order is either an omitted `price`, `price: 1` on a buy, or `price: 0` on a sell. **We never send market orders.** Every order is a limit order.
- **NO is the complement of YES within a single book.** `sell yes q@p` ≡ `buy no q@(1−p)`. A sell that is
  flat, opposite to the held side, or larger than the position is *canonicalized* by the engine into the complement buy,
  and the response reports the canonical `action/side/price/quantity`.
- Buying the side opposite to a held position nets FIFO and pays 1 per cancelled pair immediately.
- **Self-trade prevention is enforced server-side.** The crossing remainder is cancelled with
  `terminalReasonCode: "SelfTradePrevented"`. We *also* refuse self-crossing orders in our OMS, so we don't burn requests.
- Winning share pays **1 SUSQie**. Internally we hold prices as integer **pips (1e-4)** with `TICK = 50 pips`.

## Endpoints we call

Tournament context: `GET /tournaments/midterm-elections` returns `id` (UUID), `memberCount`, `marketCount`, and `myBalance`.
Every call below passes that `id` as `tournamentId`, or uses the `/tournaments/{slug}/…` form.

| Purpose | Call | Notes |
|---|---|---|
| Identity | `GET /account` | `balance` is the org default tournament's balance, not necessarily ours. Informational only. |
| Tournament | `GET /tournaments/{slug}` | Field size (`memberCount`), `marketCount`, `myBalance`, `isPendingEnrolment`. |
| Market list | `GET /tournaments/{slug}/markets?status=open&limit=100&cursor=` | Cursor pagination. `exchanges[]` carry tournament `latestPrice`, which is **null until a tournament trade happens**. |
| Market detail + book | `GET /tournaments/{slug}/markets/{id}` | Embedded per-exchange `bids/asks/bestBid/bestAsk`, plus `overround` and `hasArbitrageOpportunity`. |
| Resolution tree | `GET /markets/{id}/nodes?tournamentId=` | Contract nodes (`contractType`, `contractDetails`, `settlementOptions`, `settlementDate`). This is the only machine-readable part of the rules. The Info-tab prose is **not** in the API. |
| Bulk top-of-book | `GET /exchanges/prices?ids=…(≤100)&tournamentId=` | `latestPrice/bestBid/bestAsk/spread` plus `missingIds`. Preserves request order. |
| Book | `GET /exchanges/{id}/orderbook?depth=≤200&tournamentId=` | Bids descending, asks ascending. |
| Tape | `GET /exchanges/{id}/trades?tournamentId=&limit≤200&cursor=` | `side ∈ {YES,NO}` says which space filled. |
| Candles | `GET /exchanges/{id}/price-history?resolution=1m…1w&tournamentId=` | Sparse OHLCV plus VWAP. |
| Place | `POST /orders` | Body `{exchangeId, side, action, quantity(int), price, expirationDate?, tournamentId, idempotencyKey}` (**key required**). Response: `orderId, open, remainingQuantity, quantityTraded, totalCost, fillPrice, terminalReasonCode, all{…collateral economics}`. |
| Batch | `POST /orders/batch` | ≤50 orders under one key, best-effort, returns 207 with per-item results. On 503, retry with the same key to resume. |
| Atomic | `POST /orders/multi-leg` | ≤10 legs, all or nothing. Optional `relationshipConstraint` (we **don't** send it for arbitrage legs, because it would 422 us). |
| Open orders | `GET /orders?status=open&tournamentId=&exchangeId=` | Cursor pagination. `quantity` is the executable remainder. |
| Order | `GET /orders/{id}`, `GET /orders/{id}/fills` | |
| Cancel | `DELETE /orders/{id}` | 409 = already closed (fine). 503 = retry. |
| Cancel all | `POST /orders/cancel-all {tournamentId, exchangeId|marketId}` | 200/207/422/503. On 207/422, confirm with `GET /orders?status=open` before re-quoting. |
| Positions | `GET /tournaments/{slug}/portfolio/positions` | Signed `quantity` (+YES/−NO), `avgCost`, `currentPrice` (tournament valuation price), FIFO `lots`. 409 = holdings without valuation. |
| P&L / value | `GET /tournaments/{slug}/portfolio/pnl?period=all` | `totalAccountValue` is what the leaderboard ranks on. |
| Fills | `GET /tournaments/{slug}/portfolio/fills` | |
| Settlements | `GET /tournaments/{slug}/portfolio/settlements` | win/loss/refund/correction. |
| Collateral | `GET /portfolio/collateral?tournamentId=` | ALL relationship-backed advances. See below. |
| Leaderboard | `GET /tournaments/{slug}/leaderboard?period=all&limit=100&offset=` | `total` gives the ranked field size, and `myRank`. Entries: `rank, profileId, username, pnl, tradesCount, volume, winRate, roi`. Ties rank 1-1-3. |
| Relationships | `GET /relationships?tournamentId=`, `GET /relationships/graph?marketId=` | Canonical engine graph: `monotonic, complementary, mutually_exclusive, implication, boolean_*, conditional_if, boolean_expression`, with `isExhaustive`. |
| Constraints | `GET /relationships/constraints?violationsOnly=true&minViolation=0.01&tournamentId=` | `violationAmount`, `bound/lowerBound/upperBound/currentValue`, `observedPrices`, `suggestedCorrectiveTrades` (exchange, side, action; **no size**). |
| Realtime | `POST /realtime/token` | Returns `{token, expiresAt(3h), supabaseUrl, anonKey, channels.user}`. |

## Realtime (Supabase private broadcast)

Delivery is best-effort: 250 ms coalesced batches with no replay. Each batch carries `delivery.revision/previousRevision`.
A gap, reconnect, token refresh, or socket error means **refetch from REST before acting**.

| Channel | Event | Why we care |
|---|---|---|
| `relationships:violations:{tournamentId}` | `violation` | **The constraints racer is event-driven.** The payload is the same object as a `/relationships/constraints` row. This is our edge over the 120–180 s pollers. |
| `tournament:{tournamentId}` | `market_batch` | Tournament trades, `bookDirty`, and `marketSettled` hints, with `tournamentId` on each item. |
| `user:{profile_id}` | `account_batch` | Our fills, order updates, settlements, and refunds. Feeds the OMS fast path, which REST reconciles. |
| `leaderboard:all` | `tick` | Debounce, then refetch the leaderboard for telemetry. |

`price` on trades and fills is YES-denominated. **Order expiry emits nothing.**

## ALL collateral (capital efficiency)

The engine computes a **guaranteed payout floor** for each connected component of relationship-linked positions,
and advances collateral against it. Example: `NO` on two mutually exclusive outcomes always pays at least 1, so part of the
second leg's cost is backed by that floor instead of cash. The response field `all.netBuyingPowerImpact`
is authoritative. Implications:

- A `Σ YES > 1` violation on a mutually exclusive set, traded as buying NO on every leg, costs almost **no buying power**.
- Scenario-book sizing must read buying power from the engine (`/portfolio/collateral`, order responses),
  not from `Σ price × qty`.
- *Unverified until live:* whether chamber-control, race, and seat-count markets are linked in the graph for the Cup.

**Documented** (official reference): order responses (single, batch items, multi-leg legs) carry a nullable
`all` = `{fullNotionalCost, effectiveEntryCost, collateralSavings, relationshipIds, componentId,
guaranteedPayoutFloorAfter, outstandingAdvanceAfter, collateralRepayment, netBuyingPowerImpact}`, null when
the engine produced no relationship quote. `GET /portfolio/collateral?tournamentId=` lists components
`{profileId, tournamentId, componentId, coveredExchangeIds (integers), relationshipIds, outstandingAdvance,
guaranteedPayoutFloor (null when not every allowed outcome could be evaluated), diagnostic}`. The unscoped
`GET /portfolio/transactions` shows `ALL_COLLATERAL_ADVANCE / _REPAYMENT / _RELEASE` with
`outstandingAdvanceAfter`, `componentId` and `reason` (the tournament-scoped variant documents none of those
three). `account_batch.collateralChanges` pushes `{delta, outstandingAdvanceAfter, componentId, tournamentId}`.

**Modeled, not documented** (`py/susq/platform/collateral.py`, assumptions A1-A7; ported to
`crates/platform/src/collateral.rs`; used by the mock, the paper broker, and the opt-in sizing):
components are unions of active relationships sharing an exchange, id = sorted exchange ids joined by
`-`; the floor is the exact minimum payout over allowed joint outcomes (mutually exclusive, exhaustive or
not; complementary; implication/monotonic with one antecedent and consequent; other types add no
constraint, so the floor stays a lower bound; > 16 exchanges → no floor); fills are evaluated in order and
each may be advanced `min(opening cost, floor_after − outstanding)`; a fill that drops the floor below the
outstanding advance repays the difference from cash at once; settlement releases what the remaining
component can't back, out of the payout; account value nets the outstanding advance.

**How Susquatch uses it:** every `all` block is journaled (`all_economics`, with the model's prediction
alongside) whatever the config says. `[risk] use_collateral_buying_power` (default **false**) switches
buying-power checks and racer bundle budgets from full notional to the modeled effective cost, in both the
bridge and rust-live; exposure caps stay on notional. `measure-day1`'s `collateral` section compares the
engine with the model on a real linked fill and recommends flipping the flag only on a match within 0.01.
The mock simulates all of this (`--no-collateral` turns it off; `POST /_mock/settle` drives releases).

## Settlement facts that shape strategy

- Autosettlement runs every **4 h** from each market's configured source. The **four chamber-control markets are settled manually**
  once control is official, and stay **closed to trading** while they wait.
- `REFUND`/N/A returns the refundable cost of held shares.
- Final rank = SUSQie balance after *all* markets settle. Standings ties rank 1-1-3 and split the pooled prizes.

## Lock-time arithmetic

Trading closes **2026-11-04 12:00 Eastern Standard Time**. DST ends Nov 1, so that is **17:00:00Z**.
Susquatch's hard gate: **no new orders after 16:55:00Z**, a cancel-all at 16:55:00Z, and every order ever sent carries
`expirationDate ≤ 16:55:00Z`, so a dead process cannot leave orphans resting into the lock.
Trading opens **2026-10-01 12:00 EDT = 16:00:00Z**.
