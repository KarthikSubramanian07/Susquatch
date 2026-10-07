# EXP-004: Fit the winning-multiple threshold to the trade tape, not the leaderboard

- **Proposed:** 2026-10-06 by weekly-strategy (research memos of 2026-10-06)
- **Approved:** 2026-10-06 by Karthik
- **Status:** shipped

## Hypothesis
A field model fitted to the Cup trade tape (cumulative volume bought at each price on each contract, plus the
leaderboard's per-account turnover) gives a resolved-field-maximum distribution T(ω) that is stable week to week,
assigns the 10x marked accounts a resolved balance near zero, and puts T50 for N≈4,300 in the 8 to 25x range,
so that at least one scenario scores P(M_S > T_S | S) > 0.02.

## Change
Branch `exp/004-tape-fitted-threshold`. The tape itself is recorded by the bridge (PR #65).

1. `py/susq/telemetry/field.py` (new):
   - `read_tape_ladder` folds the tape one `date=` partition at a time into shares per (exchange, taker side,
     price). Peak memory is one day of trades.
   - Price space: the platform has one YES-space book and canonicalises sells into the complementary buy, so
     `side` is the side the taker bought and `price` is the YES price. `detect_price_space` votes per exchange
     (YES-taker and NO-taker VWAPs close together means YES space; summing to about one means taker space) and
     switches only on a strict majority. The votes are recorded in the footprint.
   - Footprint per contract side c: spend = shares · q with q the side price (price for YES, 1 − price for NO);
     only q ≤ 0.20 counts. `S_c = Σ spend`, `p̄_c = S_c / Σ shares`. Only the taker side counts.
   - All-in count: `stake = max(100,000, median volume of accounts with turnover ≥ 100%)`,
     `n_c = min(S_c / stake, n_hi)`, rescaled so that `Σ n_c ≤ n_hi`. Bots are accounts with
     `trades_count ≥ 200`.
   - `tape_threshold(outcomes, contracts, footprint, params, rng, floor=…)`: per world,
     `T(ω) = max(floor, consensus/bot maximum, max over held sides that pay in ω of 1/p̄_c)`. A side is held
     with probability `min(1, n_c)`, a YES side pays when its leg resolves YES and a NO side when it resolves NO,
     and a side whose exchange is not simulated pays independently with probability p̄_c. The YOLO share of
     `FieldParams` is dropped; passive, consensus and bots use `tourney.consensus_and_bot_max` (factored out of
     `simulate_threshold` with the same draw order, so the SPEC model's output is unchanged).
   - Floor: the leaderboard anchor is kept only as a floor, namely the third-highest mark, at most the EXP-001
     fit's T50.
2. `run_telemetry` writes the footprint under a new `field` key in `telemetry.json`: aggregates per contract side
   (cheap spend, shares, p̄, n, trade count), never trade ids or accounts.
3. `run_targets` uses the tape threshold when the footprint is usable (at least 1,000 trades spanning at least 2
   days with at least 5 cheap contract sides), the EXP-001 leaderboard fit otherwise (the notes say why the
   tape was not used), and the SPEC field model when neither exists. The notes start the threshold clause with
   `T from tape(...)`, `T from leaderboard+prior(...)` or `T from field-model(...)`.
4. Robustness (`optimizer/book.py`): scenarios are also ranked under constant thin (1.5x), medium (5x) and tail
   (20x) thresholds. The fitted-T winner is committed only if it is top-ranked under at least two of the four
   rankings. Otherwise the scenario top-ranked under the most rankings is committed if that count is at least
   two (ties go to the better fitted rank). Otherwise nothing is committed. Legs and quantities always come
   from the fitted T. The notes record each assumption's top scenario and which ones the committed scenario
   wins.
5. Tests in `py/tests/research/test_field.py`: aggregation on a synthetic tape (both sides, cheap and expensive
   prices, repeats, invalid rows), price-space detection, per-day folding, n estimation and its caps,
   usability, the jump to 1/p̄ only where the heavy cheap leg pays, the 0.95 marked-account case, fractional
   and unsimulated sides, parity with the SPEC model without YOLO, the floor, the robustness rule, and the
   pipeline with and without a footprint.

Quick check on a synthetic field shaped like the Oct 6 aggregates (N 4,350, 1,263 ranked, 496 accounts with
turnover ≥ 100%, 25 bot-like accounts, 120 all-in YOLO accounts on cheap sides chosen ∝ price, five whole
balances at 0.95 on ~1% outcomes, 240 contracts with a one-factor copula): T10/T50/T90 = 8.5 / 14.9 / 29.5x.
The 0.95 buyers contribute nothing. Without bots T10 drops to 6.7x and T50/T90 barely move (14.1 / 29.5x).

## Metric & window
Weekly from Oct 11 to Nov 1: T50/T90 per field assumption, the robust scenario's P(M>T|S), and the week-to-week
change in T50 (win: < 30% change with no new information). Post-resolution: the realised field maximum versus the
model's p50/p90 (win: inside the band).

## Result
(pending)

## Decision
(pending)
