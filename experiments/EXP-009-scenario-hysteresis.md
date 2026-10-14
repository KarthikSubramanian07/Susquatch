# EXP-009: Scenario hysteresis

- **Proposed:** 2026-10-11 by weekly-strategy
- **Approved:** 2026-10-11 by Karthik
- **Status:** running

## Hypothesis
Near-tied scenarios swap places run to run: house D and house R alternated four times between Oct 7 and Oct 9, and the
red-state-reach sign flips between the 07:38Z targets and a dry run 90 minutes later (closeness 0.00071 and 0.00077).
The executor never reduces a leg that drops out of the targets, so each flip after buying starts leaves an orphaned
leg and spends cash on the challenger's. Keeping the incumbent scenario unless a challenger is clearly better removes
the churn and costs under 10% of closeness.

## Change
In `py/susq/calibrate/pipeline.py`, pass the scenario id of the current targets file into `build_book`. The optimizer
keeps it as `best` unless the top challenger's closeness (or score, once nonzero) exceeds the incumbent's by more than
`[optimizer] switch_margin = 0.25`, or the incumbent is top under none of the four threshold assumptions. The targets
notes record "kept incumbent" or "switched (margin x)".

## Metric & window
Oct 12 to Nov 2: the number of scenario switches in targets files (target at most 1), and the incumbent's closeness
divided by the best challenger's at each run (target at least 0.8 on every run). Revert if a switch is blocked while
the challenger's P(M>T|S) is nonzero and the incumbent's is zero.

## Result
Interim, 2026-10-14: the rule did not hold. From Oct 12 13:30Z to Oct 13 19:58Z the targets switched scenario five
times, every time on the escape clause ("top under none of the four threshold assumptions"), never on the margin. With
four near-tied N9 variants, Monte Carlo noise is enough to leave the incumbent second under every assumption. The live
book now holds legs from three scenarios: TN-05 D, SC Governor D and Rhode Island Senate D are current; Alabama
Governor D and Louisiana Senate D are orphaned (about 1,500 at cost). Fix (PR #86): the escape clause now fires only
when the incumbent is more than `switch_margin` behind the top scenario under every assumption, using the same
measure as the margin test. This must be live before the book grows to full size on Oct 21.

Second leak, found while verifying PR #86 on the VM: with the scenario kept, the legs still changed, because legs are
ranked by in-scenario probability per SUSQie at the current ask and a one-tick move reorders near-equal legs. The
06:07Z run kept the scenario but replaced SC Governor D (19,418 shares held) with a new leg. Fix (PR #87): legs of the
current targets file rank as if `switch_margin` more valuable, and the T3 idiosyncratic leg is kept while it still
qualifies.

## Decision
