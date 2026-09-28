# API_NOTES — The Super Market API (Predictions Cup)

**Official API docs: https://sig.thesuper.market/api/v1/docs. They override everything below.**

**Status: UNOFFICIAL. Verify every line against the official reference before live use.**

- Official reference: https://sig.thesuper.market/api/v1/docs. It blocks some automated fetchers. From your machine, try `curl -sL` with a browser User-Agent, and look for an OpenAPI/JSON link on that page (e.g. `/api/v1/openapi.json` is a guess to probe, not a known path). If you still can't read it, ask me to save the page into `brief/api/`.
- Source of these notes: a public competitor's working Python client, `github.com/ZVogel1/SIG-PM-Challenge-F2026`, file `src/pmcup/client.py`, commit `38abf18` (Sep 25, 2026). The author wrote it against the official docs. We read it only to learn the API shape. **Do not copy their code or strategy.** Write our own Rust client.

## Base URL and auth

- Base URL in their config: `https://www.thesuper.market/api/v1`. **Note the host differs from the docs host `sig.thesuper.market`.** Probe both with `GET /account` and use whichever the official docs specify.
- Auth header: `Authorization: Bearer <API_KEY>`. Also send `Accept: application/json` and `Content-Type: application/json`.
- Create the key at sig.thesuper.market → Settings → API Keys, with scopes **read + trade**.
- They retry on 408, 425, 429, 500, 502, 503, and 504 with exponential backoff. Rate-limit numbers are unknown. Read the 429 / `Retry-After` responses.

## Endpoints observed

| Method | Path | Params / body | Notes |
|---|---|---|---|
| GET | `/account` | — | identity / balance |
| GET | `/tournaments` | `status=any`, `limit=100` | returns `data[]` with `id`, `slug`, `name`, `myBalance`, `initialBalance` |
| GET | `/tournaments/{slug}` | — | tournament detail; slug is `midterm-elections` |
| GET | `/tournaments/{slug}/markets` | `status=open`, `limit`, `cursor` | paginated: `data[]`, `pagination.hasMore`, `pagination.nextCursor` |
| GET | `/markets` | `tournamentId`, `status`, `category` (e.g. `election-outcome`), `limit`, `cursor` | alternative market listing |
| GET | `/exchanges/prices` | `ids` (comma-separated exchange IDs, ≤100 per call), `tournamentId` | `data[]` with `exchangeId`, `latestPrice`, `bestBid`, `bestAsk`, `spread` |
| GET | `/exchanges/{exchangeId}/orderbook` | `depth` (e.g. 10), `tournamentId` | L2 book |
| GET | `/relationships/constraints` | `violationsOnly=true/false`, `minViolation` (e.g. 0.01), `tournamentId` | **the platform itself computes cross-market constraint violations**: rows with `evaluationStatus`, `violationAmount`, `reason`, `suggestedCorrectiveTrades[]` (`exchangeId`, `marketId`, `marketTitle`, `outcomeSide`, `action`, `currentPrice`, `rationale`) |
| GET | `/tournaments/{slug}/portfolio/positions` | — | positions: `exchangeId`, `quantity`, `currentPrice`, `settled`, P&L fields (`totalUnrealizedPnl`, `totalMarketValue`, `unrealizedPnlPct`) |
| GET | `/portfolio/positions` | — | non-tournament positions |
| GET | `/tournaments/{slug}/leaderboard` | `period=all`, `limit` | `leaderboard[]`, `myRank` |
| POST | `/orders` | body below | place order |

**Order body** (JSON):

```json
{ "exchangeId": "…", "side": "yes|no", "action": "buy|sell", "quantity": 10,
  "price": 0.45, "tournamentId": "…", "idempotencyKey": "uuid" }
```

- `price` is a **decimal probability in 0–1** (e.g. 0.45). It is optional in their client, which may mean market orders exist; verify.
- Tick size, bounds, time-in-force, post-only/IOC, cancel/replace endpoints, and open-order listing were **not seen** in their client. Find them in the official docs.
- `idempotencyKey` is supported. Use deterministic keys per intent so retries never double-place.

## Data model

- A **market** has `id`, `title`, and `exchanges[]`. Each exchange is a tradable outcome with `id`, `option` (e.g. YES), and `latestPrice`.
- Binary markets typically expose the YES exchange. Their code treats buying NO as roughly equivalent to selling YES at the bid (NO ask ≈ 1 − YES bid). Confirm whether NO is a separate book or the complement.
- Parties have separate markets per race, e.g. #381 Dem-NH-Senate and #382 Rep-NH-Senate.

## Strategy implications (update to SPEC)

1. **Constraint arbitrage is crowded.** The platform publishes `/relationships/constraints` with suggested corrective trades to every API user, and at least one visible competitor trades it directly. Treat it as a **speed race**: poll it at the maximum safe rate, act within one cycle, and size small. Don't count on it as a durable edge. It's still worth running, because slower bots (their default cycle is 120–180 s) leave windows of seconds to minutes.
2. **Competitor cadence.** One known bot decides every 120–180 s and places ≤5 orders per cycle. A faster poll plus a resting-quote strategy can pick off stale orders between their cycles.
3. **Prices are decimals.** Build the Rust `Price` type as integer ticks internally once you confirm the tick size (probably 0.01), and convert at the API boundary.
4. **Verify on Oct 1** (add to `measure_day1`):
   - host;
   - tick size and bounds;
   - order types;
   - cancel endpoint;
   - open-orders endpoint;
   - how NO works;
   - rate limits;
   - position limits;
   - how `currentPrice` marks positions for account value;
   - whether constraint rows include the Sponsor's own quotes.
