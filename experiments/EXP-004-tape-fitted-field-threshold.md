# EXP-004: Fit the winning-multiple threshold to the trade tape, not the leaderboard

- **Proposed:** 2026-10-06 by weekly-strategy (research memos of 2026-10-06)
- **Approved:** 2026-10-06 by Karthik
- **Status:** proposed

## Hypothesis
A field model fitted to the Cup trade tape (cumulative volume bought at each price on each contract, plus the
leaderboard's per-account turnover) gives a resolved-field-maximum distribution T(ω) that is stable week to week,
assigns the 10x marked accounts a resolved balance near zero, and puts T50 for N≈4,300 in the 8 to 25x range,
so that at least one scenario scores P(M_S > T_S | S) > 0.02.

## Change
1. Bridge records the tape (`data/cup/tape`, PR to follow from feat/cup-trade-tape).
2. `py/susq/telemetry/field.py`: per contract j, money spent on each side at prices ≤ 0.20 → an empirical YOLO
   weight π_j and an all-in count n_j = min(spend_j / 100,000, accounts with turnover ≥ 100%). T(ω) is sampled as
   the maximum over contracts paying in ω of per-account resolved multiples, with `tourney.FieldParams` seeded from
   those counts instead of the α-price prior. The leaderboard anchor (`fit_threshold`) is kept only as a floor.
3. Targets note records which of three field assumptions (thin 1.5x, medium 5x, tail 20x) the chosen scenario is
   robust to; the optimizer ranks under all three and refuses to commit a leg that is top-ranked under fewer
   than two.

## Metric & window
Weekly from Oct 11 to Nov 1: T50/T90 per field assumption, the robust scenario's P(M>T|S), and the week-to-week
change in T50 (win: < 30% change with no new information). Post-resolution: the realised field maximum versus the
model's p50/p90 (win: inside the band).

## Result
(pending)

## Decision
(pending)
