# EXP-013: Size the scenario book to what risk can hold, and stop eligibility flapping

- **Proposed:** 2026-10-25 by weekly-strategy
- **Approved:** 2026-10-25 by Karthik (asked to do anything within the rules to maximise P(1st))
- **Status:** running (approved 2026-10-25; Karthik asked to do anything within the rules to maximise P(1st))

## Hypothesis
The targets ask for a book that risk will not let the account hold. At full size, the optimizer sizes legs at up to
`[optimizer] max_leg_frac` 0.12 (about 10,000 per leg, 61,499 in total). `[risk] per_market_gross_frac` 0.05 stops
each market at about 4,900, and `total_gross` (0.854 of 0.90 used), `region:south` (0.386 of 0.40) and `house` (0.189
of 0.20) refused every scenario order from Oct 19 to 25, so the book reached 14% of target at cost (8,412). The
remaining churn comes from the consistency test: legs at P(YES | S) = 0.30 sit exactly on
`min_leg_prob_in_scenario`, and with about 560 draws in S the in-S probability has a standard error of about 0.02. That
flipped LA Senate D, KY-04 D and Tennessee Governor D in and out 10 times after the EXP-010 leg lock shipped. If we
size legs to the per-market cap, size the book to the room the caps leave, and keep a current leg while its in-S
probability stays above 0.30 × (1 − `switch_margin`) = 0.225, then the targets will match what is held, scenario
refusals will drop to about zero, and core leg changes will be zero.

The weekly review also finds that a bigger book lowers P(1st) if election night is the path to first place (about 0.10
falling to 0.06 under a naive model at a 0.6 hit rate). So this proposal sizes the book down to what it holds, about
15%, rather than freeing the caps so it can grow.

## Change
- `config/susq.toml` `[optimizer]`: `book_frac = 0.15` (was 0.60), `max_leg_frac = 0.05` (was 0.12, now equal to
  `[risk] per_market_gross_frac`).
- `py/susq/optimizer/book.py` `_lock_legs`: current legs (tier 0) count as eligible while
  `cond >= min_leg_prob_in_scenario * (1 - switch_margin)` and `cond >= min_lift * uncond`. New legs still need the full
  0.30. The targets notes record "keep-band kept ..." when a leg is kept on the band.
- No risk cap changes. No changes to the bridge or rust-live.
- Dry run on the VM, 2026-10-25 about 15:45Z (`write=False`, scratch state): at 30% with 5% legs, the same scenario
  wins with 6 legs costing 31,009 (closeness 0.148 against 0.222 at 60%). P(1st) is 0 in every variant.

## Metric & window
From deploy to the Nov 2 cutover:
- Share of `scenario_order` intents refused on caps. Win: under 10% (it was 100% on Oct 19 to 20 and 71 to 98% on
  Oct 21 to 25).
- Core leg changes in targets files while the scenario is unchanged. Win: 0.
- New orphaned YES positions. Win: 0.

Revert if a kept-on-band leg's ask goes above its in-S probability.

## Result

## Decision
