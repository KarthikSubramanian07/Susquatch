# EXP-005: Regional polling-miss scenarios and the 2022-prior detector

- **Proposed:** 2026-10-06 by weekly-strategy (literature review 2026-10-06)
- **Approved:** 2026-10-06 by Karthik
- **Status:** proposed

## Hypothesis
Scenario cells indexed by regional polling-miss patterns (2018: Midwest/FL R-miss with Southwest D-miss; 2022:
near-zero bias with markets too Republican) with Shirani-Mehr magnitudes (shared bias ≈ 2 pts, RMSE ≈ 3.5 pts)
produce a scenario book that is less correlated with the field's exposure than the national-decile cells, and the
Cup field in 2026 carries the 2022 "polls underestimate Republicans" prior (Cup Senate control more Republican than
Silver Bulletin, DDHQ and FiftyPlusOne while the generic ballot is D+6 or better).

## Change
1. `py/susq/optimizer/scenarios.py`: two named cells added to the grid, built from `sim` draws by conditioning on
   the signed regional shocks; their P(S) comes from the calibrated sim, not hand-set.
2. `py/susq/telemetry/detector.py` (weekly job): Cup chamber-control and ten key-race mids versus the three public
   models (fetched from their public pages, cached); writes `state/research/detector.json` and a line in the daily
   report. No trading from it; it feeds the Sunday review.
3. Leg filter in `optimizer/book.py`: a leg priced 0.05 to 0.15 must have P(YES | S) ≥ ask / 0.75.

## Metric & window
Sundays Oct 11 to Nov 1: field exposure e_S of the chosen scenario (win: |e_S| lower than the national-cell choice);
detector gap in points (reported, not scored). Post-resolution: did the chosen regional cell contain the realised
world (yes/no).

## Result
(pending)

## Decision
(pending)
