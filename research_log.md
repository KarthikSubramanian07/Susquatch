# Research log

Timestamped, append-only. A scrubbed copy of this file feeds the write-up, so it contains **no raw Competition Data**:
no order IDs, fills, or other players' names. Aggregates are fine.

## 2026-09-28

- **16:45 PT.** Read the brief end to end. Pulled the official API docs: `sig.thesuper.market/api/v1/docs` is a Scalar page with the whole
  OpenAPI 3.1 document inlined, so `scripts/fetch_api_docs.py` extracts it verbatim. It has 102 operations, 67 of them
  DMM/admin-only. The unofficial notes were close on shape but wrong on tick size (0.005, not 0.01), and they missed realtime,
  GTD expirations, batch/multi-leg orders, server-side STP, and the ALL collateral model. All of these are logged in `brief/ERRATA.md`.
- **16:50 PT.** Both `www.` and `sig.` hosts return the documented error envelope unauthenticated (Vercel, `x-request-id`).
  No rate-limit headers before auth.
- **Design consequence.** Realtime `relationships:violations:{tournamentId}` pushes violations in about 250 ms, while known competitor
  bots poll every 120–180 s. The constraints racer should be event-driven. GTD expirations give a dead-man switch for free,
  so every order carries one.
