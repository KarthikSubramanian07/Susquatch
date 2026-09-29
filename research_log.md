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

## 2026-09-29

- **Build day, in parallel.** Six components built on separate branches, each merged through a PR with CI:
  platform + mock, bridge, rust-live, C++ models, research, election night, ops + dashboard. All 16 PRs merged.
  Integrated `main` runs 1,178 tests green: Python 609, Rust 416, C++ 101, dashboard/Worker 52.
- **Calibration sanity check.** Calibrating race marginals to public Kalshi/Polymarket mids gives **59.2–59.5 %** for Senate
  control from correlation alone, against **62 %** on the chamber market. That's inside the SPEC's 2–4 point band, untuned.
- **Interop run found a real bug.** The release `susq-live` binary against the Python `susq-mock`, two independent implementations of the
  same docs, worked end to end: tournament resolution, 4 realtime channels, violation detected within 1 s, time gate respected pre-open.
  With the open moved to the past, the racer fired **16 bundles in 20 s** on one standing violation, because paper fills don't consume liquidity.
  Both racers now have a re-fire cooldown and a per-relationship exposure cap (#12). Lesson: a guaranteed payoff isn't a reason to trade without limit.
- **Data-source corrections:** CA SoS test dates (E18), Georgia's move off Clarity (E19), a Kalshi ticker/title mismatch (E20).
- **Needle.** Interval coverage is nominal over 300 synthetic mail-first nights, and 3,000 counties recompute in 0.8 ms.
  Confirmed count corrections now flow from enr to needle (#14) instead of being dropped as regressions.
