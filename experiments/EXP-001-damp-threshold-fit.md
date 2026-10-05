# EXP-001: Damp the winning-multiple threshold fit

- **Proposed:** 2026-10-05 by weekly-strategy
- **Approved:** (pending) by Karthik
- **Status:** proposed

## Hypothesis
Fitting the growth term on the top-3 balance instead of the top-1, and capping T50 at the SPEC §6 prior's upper band for
the observed field size, brings T50 from 93x into the 20 to 40x range without moving it when the field really does run away.

## Change
`py/susq/models/tourney.py` (and the `log T50` blend described in docs/RESEARCH.md): `g` from the 7-day least-squares
growth of the third-highest multiple; `T50 <= prior_hi(N_observed)`; p90 spread unchanged. PR to follow.

## Metric & window
T50 and T90 in each daily targets file from approval through Oct 21. Win: T50 within 20 to 40x on every day where the
observed top-1 stays under 15x, and the ranked scenarios have P(M>T|S) > 0 for at least one scenario.

## Result
(pending)

## Decision
(pending)
