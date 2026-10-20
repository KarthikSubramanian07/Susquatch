# EXP-010: Leg lock for the scenario book

- **Proposed:** 2026-10-20 by human (status review)
- **Approved:** 2026-10-20 by Karthik (asked for improvements; extends the approved EXP-009 goal of no churn)
- **Status:** running

## Hypothesis
With the scenario fixed, the scenario book's legs still change run to run, because the EXP-009 boost (+25% on value
per SUSQie) is smaller than the moves in ask and fair between runs. From Oct 14 06:13Z to Oct 20 13:30Z the targets
kept the same scenario on every run but changed their core legs 11 times over 10 distinct leg sets. The executor
never reduces a dropped leg, so the account now holds Alabama Governor D (23,857 shares), Louisiana Senate D
(23,205), Tennessee Governor D (21,737) and South Carolina Governor D (19,418), none of which is a current leg.
Locking the legs stops new orphans before the book triples in size on Oct 21 16:00Z.

## Change
`py/susq/optimizer/book.py`: after the scenario is committed, its book is rebuilt with legs in three tiers, each
ranked by value: legs of the current targets file, then legs the live account holds YES shares in, then the rest.
Every current leg that is still consistent with the scenario stays in the book (concentration may not drop it).
`py/susq/calibrate/pipeline.py` reads the held legs from `state/status.json` (ignored when not live or older than
6 hours). The targets notes record "leg lock kept ...". Scenario selection itself is unchanged.

Dry run on the VM's data on 2026-10-20 (patched code in a separate container, nothing written): at the early size the
book keeps KY-04 D and TN-05 D, as the live targets do. At full size it keeps both and adds FL-16 D, Louisiana Senate
D and South Carolina Governor D, which the account already holds, instead of new legs.

## Metric & window
Oct 21 to Nov 2: the number of core leg changes in targets files while the scenario is unchanged (target 0, except
when a leg stops being consistent with the scenario), and the number of new YES positions opened by the scenario
executor in legs that are later dropped (target 0). Revert if a locked leg stays in the book while its ask is above
its in-scenario probability.

## Result

## Decision
