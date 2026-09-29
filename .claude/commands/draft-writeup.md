---
description: Draft the public write-up from the scrubbed research log, experiments and aggregate results
argument-hint: "[section to (re)draft | all]"
---

Draft `docs/WRITEUP.md`, the public write-up for SIG recruiting and the eventual public repo.

## Inputs
- `research_log.md`, `experiments/*.md`, `docs/*.md`, `PLAN.md`, `brief/ERRATA.md`
- **Aggregate** results only: final rank and field size, the balance multiple, per-strategy P&L totals, and drill statistics.

## Hard rules (Official Rules §14: Competition Data belongs to the Sponsor)
- Leave out raw orders, fills, order ids, per-trade timestamps, other players' usernames, and per-market position listings.
  Leave out any model trained on platform data and any chart built from platform tick data.
- Code, methodology, the simulator, external-data backtests (Kalshi, Polymarket, public election results), and aggregate results are OK.
- Add a banner at the top: *"Draft. Email predictionscup@sig.com before publishing anything derived from platform data."*

## Structure
1. The problem: a top-heavy tournament where rank depends on resolved payoffs.
2. The thesis: latency and under-exposure beat direction.
3. Architecture: three languages, never linked, one schema, fail-safe without a pager.
4. What happened, by phase (forage → stalk → emerge), with the honest numbers.
5. What worked, what didn't, and what the errata taught us.
6. What I'd do differently.

Tone: sharp, honest, specific, with personality and no hype. Output the sections drafted and anything withheld for data-ownership reasons.
