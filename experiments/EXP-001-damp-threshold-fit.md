# EXP-001: Damp the winning-multiple threshold fit

- **Proposed:** 2026-10-05 by weekly-strategy
- **Approved:** 2026-10-06 by Karthik
- **Status:** shipped

## Hypothesis
Fitting the growth term on the top-3 balance instead of the top-1, and capping T50 at the SPEC §6 prior's upper band for
the observed field size, brings T50 from 93x into the 20 to 40x range without moving it when the field really does run away.

## Change
`py/susq/telemetry/leaderboard.py` (the `log T50` blend described in docs/RESEARCH.md; `tourney.py` only supplies the
prior and is untouched):

- `summarize` now records the third-highest multiple per snapshot and fits `g` as the 7-day least-squares growth of its
  log, instead of the top-1. The summary's `growth_per_day` field keeps its name and now carries that value.
- `fit_threshold` keeps the blend `log T50 = (1−w)·log prior50(N) + w·log max(top1, top1·e^{g·days_to_lock})`, then
  caps `T50 <= prior90(N_observed)`. When the cap binds the method string gains a `+cap` suffix. The p90/p50 spread is
  still the prior's, so T90 = T50 · prior90/prior50, which means a capped fit reports T90 = prior90²/prior50.
- The function signature is unchanged. The one existing test that asserted an uncapped 40x at N = 2,000 (where the
  prior's p90 is 30x) now asserts 25x, because that assertion was the behaviour this experiment replaces.
- New tests: a flat 10x top-1 over a 2x top-3 keeps T50 inside the prior band for N = 4,262; a field whose top-3 is
  growing raises T50 monotonically and stops at the cap; and a top-1 that jumps to 10x and sits there contributes
  nothing to `g` at the `summarize` level.

Quick check with today's numbers (N = 4,262, top-1 9.9x flat, 5 days observed, 29 days to lock): the prior band is
19.1x to 52.8x, the fit gives T50 = 15.1x and T90 = 41.8x, uncapped. Before this change the same inputs with the
top-1 growth term gave T50 ≈ 93x.

Branch `exp/001-damp-threshold-fit`; PR to follow.

## Metric & window
T50 and T90 in each daily targets file from approval through Oct 21. Win: T50 within 20 to 40x on every day where the
observed top-1 stays under 15x, and the ranked scenarios have P(M>T|S) > 0 for at least one scenario.

## Result
(pending)

## Decision
(pending)
