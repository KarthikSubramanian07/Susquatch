# `susq-mock`: a faithful fake of The Super Market

A deterministic, in-memory exchange that speaks the official `/api/v1` contract closely enough that the
Python bridge and `rust-live` can be tested end to end without an API key. It is shared test
infrastructure: Python tests embed it over ASGI, the Rust team runs it as a process.

```bash
uv run susq-mock --port 8787                       # default seed 7, key mock-key-alpha
uv run susq-mock --seed 11 --rate-limit-rps 5 --random-503 0.02 --latency-ms 40
curl -s localhost:8787/api/v1/tournaments/midterm-elections -H 'Authorization: Bearer mock-key-alpha'
curl -s localhost:8787/_mock/state | jq .fair      # the hidden fair values (it's a mock)
```

| Flag | Default | Meaning |
|---|---|---|
| `--seed` | 7 | Everything (fairs, stale orders, the field) derives from it |
| `--api-key KEY[=USERNAME]` | `mock-key-alpha=trader-alpha` | Accepted keys, repeatable; each is its own account with 100 000 SUSQies |
| `--rate-limit-rps` | off | Per-key sliding-window limit, then `429 RATE_LIMITED` + `Retry-After` |
| `--retry-after-s` | 1 | `Retry-After` value on 429 |
| `--random-503` | 0 | Probability of a random `503 SERVICE_UNAVAILABLE` on any API call |
| `--latency-ms` | 0 | Added to every API call |
| `--sponsor-half-width` | 0.01 | Sponsor quotes `fair ± half-width`, tick-rounded outward |
| `--sponsor-size` | 500 | Sponsor size per side |
| `--players` | 40 | Synthetic leaderboard entrants |
| `--no-collateral` | off | Turn the ALL collateral model off: plain cash accounting, `all: null`, empty `/portfolio/collateral` |

Test keys must match the gitleaks allowlist pattern `mock-key-*`.

## What's in the world

- One tournament, slug `midterm-elections`, with a stable UUID (`uuid5` of the slug).
- 19 binary markets (one exchange each, market id `m`, exchange id `m + 1000`):
  - Senate D/R pairs for OH, IA, ME, TX, MI, NH, GA (**exhaustive** `mutually_exclusive` relationships);
  - Nebraska D/R with a strong independent (**non-exhaustive** `mutually_exclusive`: both can resolve NO);
  - House control, Senate control, and "Democrats hold 52+ Senate seats" (an **implication**: 52+ ⇒ control).
- Opening prints so every exchange has a tournament `latestPrice` (the valuation price).
- **The Sponsor** (a bot account) quotes both sides around a hidden fair, post-only (it never crosses
  a resting order), and re-quotes whenever it's filled.
- **Stale student orders**: a few orders 5 points through fair (bids above / asks below) that the
  Sponsor quotes around. Sniper food.
- A synthetic field with realistic spreads of account value, including exact ties at 100 000.

## Endpoints (under `/api/v1`)

Auth is `Authorization: Bearer <key>` or `X-API-Key: <key>`; missing → `401 MISSING_API_KEY`,
unknown → `401 INVALID_API_KEY`. Every response carries `x-request-id`. Errors use the official
envelope `{"error": {"code", "message", "details"?}}`.

| Endpoint | Notes |
|---|---|
| `GET /account` | `balance` = tournament cash |
| `GET /tournaments`, `GET /tournaments/{slug}` | `id`, `memberCount`, `marketCount`, `myBalance` |
| `GET /tournaments/{slug}/markets` | cursor pagination (`limit` ≤ 100), `status`, `search` |
| `GET /tournaments/{slug}/markets/{id}` | embedded book per exchange (`bids/asks/bestBid/bestAsk`) |
| `GET /markets/{id}/nodes` | single contract node; `contractDetails` has `office/state/party` |
| `GET /exchanges/prices?ids=` | ≤ 100 ids (else 400), `missingIds`, request order preserved |
| `GET /exchanges/{id}/price`, `/orderbook`, `/trades`, `/price-history` | YES space; trades newest first |
| `POST /orders`, `/orders/batch`, `/orders/multi-leg` | see semantics below |
| `GET /orders` | `status=open|closed|expired|all`, `exchangeId`, `marketId`, cursor |
| `GET /orders/{id}`, `GET /orders/{id}/fills`, `DELETE /orders/{id}` | 403 other user's, 404 unknown, 409 already closed |
| `POST /orders/cancel-all` | `{tournamentId, exchangeId|marketId}`; both → 400 |
| `GET /tournaments/{slug}/portfolio/positions`, `/pnl`, `/fills`, `/settlements` | signed qty, FIFO lots, valuation = last tournament trade; holdings without a price → 409; `totalAccountValue` nets the outstanding ALL advance |
| `GET /portfolio/collateral` | one row per component you touch: `componentId`, `coveredExchangeIds`, `relationshipIds`, `outstandingAdvance`, `guaranteedPayoutFloor`, `diagnostic` |
| `GET /portfolio/transactions`, `GET /tournaments/{slug}/portfolio/transactions` | trades, settlements and `ALL_COLLATERAL_ADVANCE/REPAYMENT/RELEASE`, newest first, `type` filter; only the unscoped read carries `outstandingAdvanceAfter/componentId/reason` (as documented) |
| `GET /tournaments/{slug}/leaderboard` | competition ranking 1-1-3, `total`, `myRank` (null until you trade) |
| `GET /relationships`, `/relationships/graph`, `/relationships/constraints` | engine-style evaluations with `suggestedCorrectiveTrades` |
| `POST /realtime/token` | `{token, expiresAt(+3h), supabaseUrl, anonKey, channels.user}` |

**Stricter than production, on purpose:** every tournament-scoped call must pass `tournamentId`
(or use the slug path). Production silently falls back to the organization's default tournament;
the mock answers `400 VALIDATION_ERROR` so that bug can't survive a test run.

## Order semantics

- One YES-space book per exchange, **price-time priority**, trades at the resting price.
- Limit prices on the **0.005 tick within [0.005, 0.995]**, else `400 VALIDATION_ERROR`. Market orders:
  `price` omitted, `price: 1` on a buy, `price: 0` on a sell; unfilled remainder cancelled;
  `expirationDate` on a market order → 400.
- **Sell canonicalization:** a fully backed sell stays a sell; a flat, opposite, or oversized sell
  becomes the complement buy (`sell yes q@p` ≡ `buy no q@(1−p)`). The response reports the canonical
  `action/side/price/quantity`.
- **Netting:** buying the side opposite a position nets FIFO and pays 1 per cancelled pair.
- **Self-trade prevention:** fills other counterparties, then cancels any crossing remainder with
  `terminalReasonCode: "SelfTradePrevented"`.
- **GTD:** `expirationDate` must be in the future; expired orders vanish at read/match time and
  emit nothing on realtime (as documented). `GET /orders?status=expired` lists them.
- **Balance:** a limit order reserves the cash its opening part could cost; short →
  `400 VALIDATION_ERROR` with `details.reason = "insufficient_balance"` (the docs say "400 insufficient
  balance" without naming a code; this is our assumption).
- **Idempotency** (per key, per endpoint): same key + same payload replays the stored response;
  different payload → `409 CONFLICT`.
- **Batch:** all items validated first (any failure → 400, nothing placed); then best effort:
  200 all ok / 207 mixed / 422 all failed.
- **Multi-leg:** all or nothing (state snapshot + rollback); duplicate exchange legs → 400; failing leg
  reported in `details.leg`; `relationshipConstraint` checked (unknown/outsider/price rule → 422).

## Realtime (Supabase Realtime look-alike)

`ws://<host>/realtime/v1/websocket?apikey=mock-anon-key&vsn=1.0.0`, Phoenix JSON frames. Join
`realtime:<channel>` with `payload.config.private = true` and `payload.access_token` from
`POST /realtime/token`; heartbeat on topic `phoenix`; refresh with an `access_token` push. Expired or
forged tokens are refused on join and kicked (`system` error + `phx_close`) mid-session.

| Channel | Event | When |
|---|---|---|
| `tournament:{tournamentId}` | `market_batch` | after any trade or book change (`trades`, `bookDirty`, `delivery.revision/previousRevision`) |
| `user:{profileId}` | `account_batch` | your fills, order updates and `collateralChanges` (`delta`, `outstandingAdvanceAfter`, `componentId`) |
| `relationships:violations:{tournamentId}` | `violation` | a trade (or `/_mock/violation`) leaves a relationship violated |

## Admin API (`/_mock/*`, no auth)

| Call | Body | Effect |
|---|---|---|
| `GET /_mock/state` | | seed, clock, hidden fairs, markets, relationships, accounts, faults, realtime stats |
| `POST /_mock/reset` | `{seed?}` | rebuild the world |
| `GET/POST /_mock/faults` | any subset of the fields below | read / patch fault knobs |
| `POST /_mock/clock` | `{advanceSeconds}` or `{set: ISO}` | move the mock clock (expiry, token lifetime) |
| `POST /_mock/fair` | `{exchangeId, fair}` | Sponsor re-marks; resting student orders go stale |
| `POST /_mock/sponsor` | `{halfWidth?, size?}` | re-quote everything |
| `POST /_mock/stale-order` | `{exchangeId, side: bid|ask, through, quantity}` | a student order `through` past fair |
| `POST /_mock/violation` | `{relationshipId?, amount}` | move fairs and print trades so the relationship is violated; broadcasts `violation` |
| `POST /_mock/settle` | `{exchangeId, outcome: YES\|NO}` | settle one exchange: pays 1 per winning share, cancels its orders, releases ALL advances; an outcome that contradicts a relationship (both legs of a pair YES) → 400 |
| `POST /_mock/realtime/disconnect` | | drop every websocket |
| `POST /_mock/realtime/skip-revision` | `{channel?, count?}` | the next batch arrives with a revision gap |
| `POST /_mock/realtime/broadcast` | `{channel, event, payload}` | raw broadcast |

Fault fields: `rate_limit_rps`, `retry_after_s`, `order_status_unknown_next` (the order **is placed**,
the caller gets `502 ORDER_STATUS_UNKNOWN`, a same-key retry replays), `service_unavailable_next`
(503 without executing), `request_in_flight_next` (409 `REQUEST_IN_FLIGHT`), `cancel_all_paused_next`
(cancels half, then 503 with `details.cancelled/remaining`), `cancel_unavailable_next`,
`random_503_prob`, `latency_ms`. `*_next` counters decrement as they fire.

## ALL collateral (relationship-backed buying power)

The engine and the paper broker share one model, `susq.platform.collateral`. What the official docs
say, and what the mock **assumes** on top (each assumption is labelled A1-A7 in that module; the whole
model switches off with `--no-collateral` / `MockConfig(collateral=False)`):

| | Documented | Mock assumption |
|---|---|---|
| Component | connected set of active relationships a portfolio touches | union of relationships sharing an exchange; id = sorted exchange ids joined by `-` (A7) |
| Floor | minimum payout over every allowed joint outcome; `null` when unavailable | exact enumeration; ME (exhaustive or not), complementary, implication/monotonic understood; other types add no constraint, so the floor stays a lower bound (A5); > 16 exchanges → `null`, no advance (A6) |
| Advance | part of a fill's nominal cost backed by the floor | per fill, in order: `min(opening cost, floor_after − outstanding)` (A1) |
| Repayment | when an execution reduces the backing | at once, from cash, `outstanding − floor_after` (A2) |
| Release | `ALL_COLLATERAL_RELEASE` exists | on settlement, out of the payout, down to the remaining floor; zero once the component has settled (A3) |
| Balance | `netBuyingPowerImpact` is authoritative | reservation = opening cost − prospective advance; a fill that would overdraw cash is refused (maker cancelled / taker remainder cancelled with `InsufficientBalance`, a mock choice) |
| Account value | not documented | cash + holdings − outstanding advance (A4) |

`all` is `null` for exchanges outside every relationship (and with collateral off); for linked
exchanges it is always present, zeros included. Property tests (`py/tests/test_mock_collateral.py`)
hold these invariants through random trading and settlement: cash never negative, advance ≤ floor,
floor ≤ payout in every allowed outcome, and nothing outstanding after settlement.

## Not simulated

Market closure before settlement, `REFUND`/N/A settlements, fees, and the 250 ms batching window
(the mock broadcasts one batch per request). Settlement happens only through `/_mock/settle`.
Relationship evaluation uses last tournament trade prices; the production engine's exact price
source is unverified.

## Using it from Python tests

```python
from susq.mock.testing import open_mock_env, serve_mock

async with open_mock_env() as env:  # ASGI, frozen clock at 2026-10-01T16:00Z
    t = await env.client.get_tournament("midterm-elections")
    await env.post_admin("/_mock/faults", {"order_status_unknown_next": 1})

async with serve_mock() as live:  # real socket for websockets
    print(live.api)  # http://127.0.0.1:<port>/api/v1
```
