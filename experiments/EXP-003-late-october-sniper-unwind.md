# EXP-003: Unwind the sniper book in the last week of October

- **Proposed:** 2026-10-05 by weekly-strategy
- **Approved:** (pending) by Karthik
- **Status:** proposed

## Hypothesis
From Oct 24, selling sniper positions whose Cup bid is within 2 pts of fair converts the 72%-of-equity sniper book into
cash at an average haircut of 2 pts or less, which funds the full-size scenario book (Oct 21 onward) and the election-night
reserve without selling at today's 7-pt discounts.

## Change
New `[strategies.sniper] unwind_from_utc = "2026-10-24T16:00:00Z"` and `unwind_max_haircut_pts = 2.0`: after that time
the sniper posts sells at max(bid, fair - haircut) for held positions, sized by the same per-market limits. Code change
in the bridge sniper plus config. PR to follow; nothing ships before approval.

## Metric & window
Oct 24 to Nov 2: realized haircut per share sold (win: mean 2 pts or less), cash on Nov 3 12:00 ET as a share of equity
(win: 60% or more), no sale below fair - 2 pts.

## Result
(pending)

## Decision
(pending)
