# EXP-006: Pre-registered count-order playbook for election night

- **Proposed:** 2026-10-06 by weekly-strategy (literature review and winners memo 2026-10-06)
- **Approved:** 2026-10-06 by Karthik
- **Status:** proposed

## Hypothesis
The Cup's election-night prices extrapolate partial counts the way 2020 to 2024 markets did (Pennsylvania mail-first
then red shift; Maricopa late-early ballots Republican; Nevada late mail Democratic; California weeks), so a table
of pre-registered hour-by-hour partial-count biases per state, traded only when the Cup price moves against the
registered direction by more than the modelled uncertainty, beats the needle's unconditional threshold rule on a
2022 and 2024 replay.

## Change
1. `config/count_order.toml`: per state and office, expected vote-type order, hour-by-hour expected swing from
   first report to final, and the uncertainty band, sourced from 2022 and 2024 county timelines (docs/ELECTION_NIGHT.md §3).
2. `crates/strategies/src/en_executor.rs`: a `count_order` gate in front of buys (and a release for rolls): a buy
   in the registered direction needs needle edge alone; a buy against it needs edge plus the band.
3. Leader/contender switch: when the Layer B book has paid in the posterior and our multiple exceeds the modelled
   field maximum, the executor hedges toward consensus on open races instead of rolling.
4. Replay drill on 2022 and 2024 timelines with chaos, recorded in `reports/drills/`.

## Metric & window
Before Nov 3: replay P&L multiple of gate-on versus gate-off on both years (win: gate-on higher on both, with fewer
reversals). Nov 3 to 4: realised fills by hour and the posterior at lock, reviewed after resolution.

## Result
(pending)

## Decision
(pending)
