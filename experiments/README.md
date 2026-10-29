# Experiments

Every live change after Oct 1 ships as an experiment so that the write-up can attribute results honestly.
Copy `TEMPLATE.md` to `EXP-NNN-short-name.md`, fill in the hypothesis and metric **before** shipping, and record the decision afterwards.

| ID | Title | Status | Decision |
|---|---|---|---|
| EXP-001 | Damp the winning-multiple threshold fit | shipped | |
| EXP-002 | Fund the early scenario book from the far-quote reserve | shipped (PR #55, 2026-10-05) | |
| EXP-003 | Unwind the sniper book in the last week of October | shipped (approved 2026-10-07, unwinds from Oct 24) | |
| EXP-004 | Fit the winning-multiple threshold to the trade tape | shipped | |
| EXP-005 | Regional polling-miss scenarios and the 2022-prior detector | proposed | |
| EXP-006 | Pre-registered count-order playbook for election night | shipped (gate, lock-time rule, contender switch off) | |
| EXP-007 | Hold the scenario book until the threshold is fitted to the tape | shipped | |
| EXP-008 | Scenario book on the bridge (live account) | running (approved 2026-10-11) | |
| EXP-009 | Scenario hysteresis: keep the incumbent unless a challenger is clearly better | running (approved 2026-10-11) | |
| EXP-010 | Leg lock: keep the scenario book's legs, prefer legs already held | running (approved 2026-10-20) | |
| EXP-011 | Endgame rival model: rivals' payout vectors, rival-aware threshold (hook off) | draft, not applied | |
| EXP-012 | Concentrated longshot book scored on P(1st) against the resolution-adjusted field | reporting (commit pending Karthik) | |
| EXP-013 | Size the scenario book to what risk can hold, and stop eligibility flapping | running (approved 2026-10-25) | |
| EXP-014 | Concentrated election-night profile (contender mode), off by default | built, off; arming decided at Nov 3 pre-flight | |
| EXP-015 | Report true cash and cap use, and seed the paper run from it | running (approved 2026-10-25) | |
