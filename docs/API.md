# The Super Market API: what Susquatch actually uses

> Source of truth: the **official** reference at <https://sig.thesuper.market/api/v1/docs>
> (OpenAPI 3.1, `info.version = 1.0.0`, fetched 2026-09-28 by `scripts/fetch_api_docs.py`; re-read from the live
> `https://sig.thesuper.market/api/v1/openapi.json` on 2026-10-01, which already differs from the copy in
> `brief/api/official/openapi.json`, so treat that file as a snapshot).
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
| **Retry, same idempotency key** | `429 RATE_LIMITED` (per-account budget **or** the trading engine's own throttle; `Retry-After: 60` observed), `409 REQUEST_IN_FLIGHT` (wait ≥90 s), `502 ORDER_STATUS_UNKNOWN`, `503 TX_CONFLICT`, `503 SERVICE_UNAVAILABLE` | Exponential backoff with jitter. Honour `Retry-After`. **Never mint a new key.** |
| **Terminal** | every other 4xx (`VALIDATION_ERROR`, `ORDER_CONFLICT` "conflicts with the account's existing holdings", `INSUFFICIENT_SCOPES`, `FORBIDDEN`, `TERMS_NOT_ACKNOWLEDGED`, `RESIDENCE_UPDATE_REQUIRED`, `CONFLICT`, `RELATIONSHIP_VIOLATION`, …) | Surface to the OMS and journal it. `TERMS_NOT_ACKNOWLEDGED` and `RESIDENCE_UPDATE_REQUIRED` show up as ACTION NEEDED in the daily summary: both need the owner on the website (accept the rules; fix the Cup registration's state or territory). |
| **Unknown 5xx** | `500 INTERNAL_ERROR`, `DATABASE_ERROR` | For reads, retry with backoff. For writes, reconcile first (see OMS), then retry with the same key. |

## Rate budget (per account, since 2026-10-01)

Changelog, Oct 1: "Each account now has one API rate budget shared by all of its keys: **100 reads and 30 writes a
minute**, or 600 and 200 for DMM accounts. Creating more keys no longer adds capacity." This replaces the per-key
guess in ERRATA E16. Consequences:

- A `429 RATE_LIMITED` carries `Retry-After: 60`. The trading engine has a **separate** throttle that returns the same
  code, so a 429 is not always the account budget.
- The budget is shared by every process using our key: the Oct 1 bridge and rust-live running at the same time split
  one 100/30. Only one of them trades at a time (the lease), but reads add up.
- Prefer bulk reads. `GET /exchanges/prices?ids=` takes up to **100** exchange ids per call, so the whole 237-market
  census is three reads. Realtime books (see below) are free.
- Budget arithmetic: 30 writes a minute is one order every 2 s. A 50-order batch is **one** write.

## Prices, ticks, sides

- **No fees.** None on buying, selling, cancelling, or settlement. Rules: "There is zero cost, fee, or financial
  obligation required from any participant." Spec: "Tournaments run on virtual currency, so there are no real-money
  deposits, withdrawals, or fees." Confirmed on our first fill (Oct 1): one share at 0.26 cost exactly 0.26. The only
  cost is the bid-ask spread, because matching is peer-to-peer.
- Prices are **YES-normalized decimals in [0, 1]** everywhere: books, trades, fills, and the realtime feed.
- **Tick = 0.005.** Limit prices must lie on the tick in **[0.005, 0.995]**. Off-tick prices return `400 VALIDATION_ERROR`.
  (The unofficial notes guessed 0.01. They were wrong.)
- A market order is either an omitted `price`, `price: 1` on a buy, or `price: 0` on a sell. **We never send market orders.** Every order is a limit order.
- `quantity` is a positive integer up to 2,147,483,647. Batches take up to 50 orders, multi-leg up to 10.
  `expirationDate` is optional; "No event announces an order expiry" (each pushed book carries `nextExpiryAt`).
- The engine may **shrink an order to fit a decreased balance**; such orders report the reduced size, not what we sent.
  Whether resting orders reserve cash is **undocumented**: read buying power from the engine, not from our own ledger.
- **NO is the complement of YES within a single book.** `sell yes q@p` ≡ `buy no q@(1−p)`. A fully backed sell
  "closes conventionally". A sell that is "flat, opposite the held side, or larger than the held quantity is canonicalized
  by the engine into its complement BUY", and the response reports the canonical `action/side/price/quantity`.
- Netting: "Each opposite share you buy cancels against one share you hold, and you are paid 1 SUSQie per cancelled
  pair immediately", reducing the position at FIFO lot cost. An order the engine cannot reconcile with holdings is a
  `400 ORDER_CONFLICT`.
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
| Tape | `GET /exchanges/{id}/trades?tournamentId=&limit≤200&cursor=&from=&to=` | `side ∈ {YES,NO}` says which space filled. Pages newest-first. The bridge recorder walks it every 30 min into `data/cup/tape` (one read per exchange in steady state, `tape_max_requests_per_pass` at most). |
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

Delivery is best-effort: batches are coalesced into **one-second** windows (the Sept 28 spec said 250 ms; changed
Oct 1) with no replay. Each batch carries `delivery.revision/previousRevision`.
A gap, reconnect, token refresh, or socket error means **refetch from REST before acting**.

| Channel | Event | Why we care |
|---|---|---|
| `relationships:violations:{tournamentId}` | `violation` | **The constraints racer is event-driven.** The payload is the same object as a `/relationships/constraints` row. This is our edge over the 120–180 s pollers. |
| `tournament:{tournamentId}:market:{marketId}` | `market_batch`, `book_dirty`, `market_settled` | One topic **per market**. Tournament trades and settlement hints with `tournamentId` on each item, plus a `books` array: the full, `asOf`-versioned order book of every exchange the batch names, so a subscriber keeps books current without REST. |
| `user:{profile_id}` | `account_batch` | Our fills, order updates, settlements, and refunds. Feeds the OMS fast path, which REST reconciles. |
| `leaderboard:all` | `tick` | Debounce, then refetch the leaderboard for telemetry. |

Since Oct 1 "the tournament-wide `tournament:{tournament_id}` topic no longer carries market updates"; subscribe per
market. REST order books carry an `asOf` version, and each realtime trade has an `id` matching the REST trade tape, so
the two can be reconciled exactly. `price` on trades and fills is YES-denominated. **Order expiry emits nothing.**

Multi-outcome markets: the live spec says the N exchanges "may be mutually exclusive or independent; imported range
outcomes settle independently" (the Sept 28 copy said mutually exclusive only). The racer already keys on the
platform's relationship type and the verified market map's `independent_can_win` flag, so nothing changed in code.

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

- Trading closes **Nov 4 12:00 EST (17:00Z)**. Autosettlement runs every **4 h** from each market's configured source.
  "Chamber-control questions and any race that goes to a runoff are settled by administrators once the result is official",
  and chamber markets stay **closed to trading** while they wait.
- `REFUND`/N/A returns "the refundable cost of the held shares". A market the Sponsor **removes** pays out "at the
  tournament's valuation price". Administrators can apply **retrospective settlement corrections**, and the rules let the
  Sponsor "update the rules, outcomes, or resolution of a Market at any time, including after the resolution date or end
  date". Recounts, challenges, and certification delays fall under force majeure.
- **Valuation.** The live leaderboard ranks by account value, which is cash plus open positions at a "tournament
  valuation price" (`currentPrice` on positions). How that price is computed is **undocumented**; everyone starts at
  100,000, so account value and P&L rank identically. Interim ranks are "dynamic, provisional, and subject to change".
- Final rank = "verified total SUSQies balance as of the resolution of all Markets". No tiebreakers: ties share a rank and
  split the summed prizes. You must complete at least one trade to be ranked (done Oct 1).
- *Not verified:* what happens to a market still unresolved when the tournament ends. Admin tooling mentions a 5 h VWAP
  default unless the tournament type waits for every market; the rules' wording implies the Cup waits.
- The Sponsor "may void, reverse, or disqualify any order, trade, or position", has no obligation to correct our own
  order errors, "has no obligation to provide or maintain liquidity", may remove any bot "at any time and for any
  reason", and the number of markets "may vary throughout the Competition". Position limits exist in the rules but no
  numbers are published. The website's own endpoints "may block automated clients": use the v1 API only.

## Lock-time arithmetic

Trading closes **2026-11-04 12:00 Eastern Standard Time**. DST ends Nov 1, so that is **17:00:00Z**.
Susquatch's hard gate: **no new orders after 16:55:00Z**, a cancel-all at 16:55:00Z, and every order ever sent carries
`expirationDate ≤ 16:55:00Z`, so a dead process cannot leave orphans resting into the lock.
Trading opens **2026-10-01 12:00 EDT = 16:00:00Z**.
