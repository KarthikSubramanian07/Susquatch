# EXP-007: Hold the scenario book until the threshold is fitted to the tape

- **Proposed:** 2026-10-06 by weekly-strategy
- **Approved:** 2026-10-06 by Karthik
- **Status:** shipped

## Hypothesis
With the mark-based threshold (T50 39.9x on Oct 6 after EXP-001), every scenario scores P(M>T|S) ≤ 0.003 and the chosen
cell flips between runs on the same data (N8 at 23:29Z, N9 at 23:51Z, each with P(S) ≈ 0.006), so a 20% book bought
from Oct 7 would be spent on a tie-breaker. Waiting four days for the trade tape (PR #65) and the tape-fitted
threshold (EXP-004) buys a stable choice at a cost of at most four days of tail-price drift.

## Change
`[strategies.scenario] start_utc` 2026-10-07T16:00:00Z to 2026-10-11T16:00:00Z (the Sunday review). The early book
fraction and the Oct 21 full-size date are unchanged.

## Metric & window
Oct 7 to 11: the asks of the legs chosen on Oct 6 (exchanges 1013, 1007, 894, 896) versus Oct 11 (win: median drift
under 2 ticks). Oct 11: the EXP-004 scenario ranking is stable across two consecutive runs.

## Result
(pending)

## Decision
(pending)
