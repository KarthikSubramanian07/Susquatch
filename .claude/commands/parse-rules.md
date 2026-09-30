---
description: Parse Cup market resolution rules into state/market_map.json (draft; never auto-verifies)
argument-hint: "[market id ... | all]"
---

Parse the Predictions Cup markets' resolution rules into the structured `susq.v1.MarketMap`, which strategies depend on.
Run this once at the start and again whenever new markets appear (the daily report flags new market ids).

## Inputs
- Latest `data/cup/markets/markets-*.json`: market list and node trees, written by `susq-bridge record`.
- `state/info_tabs/<market_id>.md`: the Info-tab text pasted by Karthik. **The Info tab is not in the API.** For any market
  without a file, write down which ones are missing rather than guessing.
- Existing `state/market_map.json`, if present. Preserve every human edit and every `verified: true` row.
- The mapping draft from `uv run susq map-markets` (Kalshi/Polymarket references with confidence notes).

## Hard rules
- **Never set `verified: true`.** Only Karthik does that, after spot-checking, with `uv run susq research verify` (`--clean` approves every
  clean two-party race whole; or name race ids and market ids).
- Conservative defaults: if there's any doubt, set `independent_can_win: true`, and set `rcv`/`runoff_possible` when the state uses them.
  The constraints racer trades exhaustive bundles only when all legs are verified *and* `independent_can_win` is false.
- Chamber-control markets get `kind: "chamber"` and `manual_settlement: true` (Guide: Settlement & Payouts).
- Known structural facts: Nebraska Senate has a competitive independent (Osborn). Alaska and Maine use ranked-choice voting.
  Georgia has runoffs below 50 %. Louisiana's 2026 system is unverified, so flag it.

## Do
0. If there is no map yet, draft one from data: `uv run susq research collect && uv run susq research map-draft`. Then refine it below.
1. For each market in `$ARGUMENTS` (default `all`), fill `race_id`, `kind`, `party`, `resolution_source`, `independent_can_win`, `rcv`,
   `runoff_possible`, `manual_settlement`, `clusters`, and `notes` (quote the decisive Info-tab sentence).
2. Write `state/market_map.json` (proto3 JSON, `header.producer.name = "parse-rules"`, seq = previous + 1).
3. Validate it with `uv run python -c "from susq.contracts import from_json, ops_pb2; import pathlib; from_json(ops_pb2.MarketMap, pathlib.Path('state/market_map.json').read_text(), ignore_unknown=False); print('ok')"`
   and fix anything that fails.
4. Write `reports/rules_review.md` with `uv run susq research rules-review` (risky races first: independents, RCV, runoffs, inverted
   references, one-venue references), then add the decisive Info-tab quote for any risky row you have text for.

Output a short summary: markets parsed, markets missing Info text, and the risky rows.
