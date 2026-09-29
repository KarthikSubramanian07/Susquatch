# ERRATA: where the brief was wrong or incomplete

Live docs and observed platform behaviour beat the brief. Every disagreement is logged here with its date and source.
"Official" means the OpenAPI document embedded at <https://sig.thesuper.market/api/v1/docs> (fetched 2026-09-28).

| # | Brief said | Reality | Source | Impact |
|---|---|---|---|---|
| E1 | Tick probably 0.01 (`API_NOTES` §3) | **Tick is 0.005**, prices must be in [0.005, 0.995], and off-tick prices return 400 | Official, API description + `POST /orders` | `Price` type uses pips (1e-4) with a configurable tick. Integer ticks are 50 pips. |
| E2 | Cancel / open-order endpoints "not seen" | `DELETE /orders/{id}`, `POST /orders/cancel-all`, and `GET /orders?status=open` all exist | Official | No need to probe for them on day 1. `measure_day1` verifies behaviour, not existence. |
| E3 | Order types unknown; `price` optional might mean market orders | Omitted price, `price:1` on a buy, and `price:0` on a sell are market orders. `expirationDate` gives GTD. Batch (≤50) and atomic multi-leg (≤10) orders exist | Official | We use GTD expirations as a dead-man switch and multi-leg for arbitrage bundles. |
| E4 | NO may be a separate book | **Single YES-normalized book.** Sells are canonicalized to complement buys | Official + Guide ("Trade both sides") | The book model stores YES space only. |
| E5 | Self-match prevention must be ours | **Server-side STP** cancels the crossing remainder (`SelfTradePrevented`). Ours remains as a request-saving guard | Official + Guide | |
| E6 | Streaming "unknown" | **Supabase Realtime** private broadcast: market, tournament, user, leaderboard, and relationship-violation channels. 250 ms batches, best-effort | Official, `POST /realtime/token` | The constraints racer becomes event-driven. |
| E7 | `/relationships/constraints` is the only relationship surface | There is also the `GET /relationships` canonical graph with `isExhaustive`, `GET /relationships/graph`, and **ALL collateral** (guaranteed-payout-floor advances) | Official | New capital-efficiency mechanic. See `docs/API.md` §ALL collateral. |
| E8 | Leaderboard shape `leaderboard[]`, `myRank` | Plus `total` (field size) and per-entry `pnl, tradesCount, volume, winRate, roi`. `period=all` ranks on current portfolio value. `GET /tournaments/{slug}` has `memberCount` | Official | Field size N is directly observable, so `tourney` can use N instead of a prior. |
| E9 | Production host ambiguity (`www` vs `sig`) | `servers[0]` = `https://www.thesuper.market/api/v1`. Both hosts answer identically unauthenticated | Official + probe 2026-09-28 | Default to `www`. `measure_day1` still records both. |
| E10 | Keys created under Settings → API Keys | Confirmed. Participant keys are **organization-bound**, and omitting `tournamentId` resolves to the org default tournament | Official | Always send `tournamentId` explicitly. |
| E11 | Info-tab rules parsed from the API | **The Info-tab prose is not in the API.** Only `GET /markets/{id}/nodes` contract details are machine-readable | Official (no description field in `Market`) | `/parse-rules` works from titles, the node tree, and human-pasted Info text. |
| E12 | Settlement timing generic | Autosettlement every 4 h. **Four chamber-control markets are settled manually** and closed to trading while pending | Guide, Settlement & Payouts | Chamber legs can't be rolled once closed. Plan the late book around that. |
| E13 | "Positions locked at 12:00pm EST" | Rules also say times are EDT/EST as applicable. Nov 4 is after the DST change, so 12:00 EST = **17:00Z** | Official Rules §2, §6 | Hard gate at 16:55Z. |
| E14 | Runtime LLM agents with a $40 cap (PROMPT) | Overridden by PROMPT Step 0.5: **no API spend**. Replaced by `.claude/commands` | PROMPT Step 0.5 | |
| E15 | "The watchdog texts me" (LANGUAGES §Ops) | Overridden by OPS_FREE_AUTONOMY §B: **no alerts**, one daily summary | OPS_FREE_AUTONOMY | The watchdog writes to the daily summary and dashboard only. |
| E16 | Rate limit numbers unknown | Still unknown. Per-key, `429 RATE_LIMITED`, and batch 207/429/503 responses may carry `Retry-After` | Official | Adaptive limiter. `measure_day1` probes gently. |
| E17 | Username chosen by the player | SIG assigns generated usernames; participants can't change them. Member notification emails are disabled | Guide, Account | The leaderboard shows generated names. The profile can be made private. |
| E18 | CA SoS test data on Oct 22 / Oct 29 / Nov 1 | Those are the **2025 statewide special election's** dates (schedule v19), carried over. Every schedule (v17–v20) uses Wed E−13, Wed E−6, Sat E−3, so 2026 is **Wed Oct 21, Wed Oct 28, Sat Oct 31, 8:00–12:00 PT**. The official v21 isn't posted yet (403). | `4-testing-schedule-v17…v20.pdf` (fetched 2026-09-29) | Capture timer scheduled on those three dates (`susq-enr-capture.timer`). Confirm when v21 is posted. |
| E19 | Georgia results via Clarity/Scytl | Georgia moved to **Enhanced Voting**. The old Clarity election ids return 403 | `docs/ELECTION_NIGHT.md` | GA source uses the generic mapped-JSON adapter (tested on real 2024 data). |
| E20 | Kalshi tickers match their titles | `SENATELA-26` is titled **Kentucky**. Joining on the ticker would mis-map the race | live Kalshi pull 2026-09-28 | Mapping joins on parsed titles. Every row still needs human `verified`. |
