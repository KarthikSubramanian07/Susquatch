# EXP-011: Endgame rival model

- **Proposed:** 2026-10-25 by human (STRATEGY §4 and §5: "Oct 28, endgame rival model")
- **Approved:** (pending)
- **Status:** draft, not applied

## Hypothesis
The hourly leaderboard plus the Cup price recordings identify the top rivals' books well enough (fit R² of at least
0.5 for at least three of the top five) to say which of them pay in the same worlds as our scenario book, and scoring
scenarios against the best modeled rival's resolved multiple, instead of the tape-fitted T alone, moves the book
away from scenarios where the leaders are overexposed without lowering P(M_S > T_S | S) by more than 20%.

## Change
Branch `exp/011-endgame-rivals`. Read-only research code; nothing trades from it, and the optimizer hook is off.

1. `py/susq/telemetry/rivals.py` (new). The public tape carries no account ids, so each rival's book is inferred
   from its own leaderboard row:
   - Quiet windows: between consecutive hourly snapshots in which an account's `trades_count` did not change, its
     P&L moved only by revaluation, ΔP&L = Σ_m q_m · Δv_m, with q_m signed shares (+YES, −NO) and v_m the
     YES-space valuation price. v comes from the price recorder (book mid by default, last trade as the
     alternative; on Oct 15 to 25 the mid gave the higher R² for 18 of 22 top accounts, mean 0.64
     against 0.59).
   - Sparse fit per account: orthogonal matching pursuit over the exchanges whose price moved, recency weights
     (half-life 72 h, ten-day lookback), BIC stop, at most one position per four windows, one refit after trimming
     windows beyond six MADs. A rival is modeled when it has at least 12 quiet windows, R² ≥ 0.30 and a plausible
     book (gross marked value at most 1.5x its balance).
   - Payout vector in world ω: balance + Σ q_m (Y_m(ω) − v_m), floored at 0. Y comes from the optimizer's own
     simulator draws through `leg_outcomes`; an unmapped or unsimulated exchange resolves YES with probability
     v_m, one shared draw per exchange. Positions whose price never moved are invisible to the fit and stay at
     their mark (the upper bound); the lower bound lets that unexplained value resolve to zero.
   - Report per rival slot (R01 = best current mark other than us): mark, fit quality, longshot and unexplained
     shares, resolved p10/p50/p90 (both bounds), P(below half its mark), P(beats us), the correlation of its
     payout with ours, its lift in our committed scenario, and its five largest exposures. Field level: R (best
     modeled rival per world) for the top 5 and top 20, max(T, R) as the true-threshold estimate, how many rivals
     are positively correlated with us and exposed to our scenario, and the cash floor per place (the multiple
     that beats all but k − 1 modeled rivals).
2. `susq research rivals` (`py/susq/telemetry/rivals_job.py`, `py/susq/scheduler.py`): writes
   `reports/rivals/rivals-<stamp>.json` and `.md` (aggregates, rivals as slots, no account names or ids) and, with
   `--book-out`, the slot books (exchange ids and shares) to an on-box path. Our own book comes from the live
   `state/status.json`, else from the current targets file.
3. Optimizer hook (off): `[optimizer] rival_decorrelation = w` (default and config value 0.0). When w > 0,
   `run_targets` reads `state/research/rivals_book.json` (ignored if missing or older than 36 h) and scores
   scenarios against T_w(ω) = (1 − w) · T(ω) + w · R(ω), R floored at 1.0; w = 1 is P(beat every modeled rival).
   With w = 0 the code path is not entered; a test proves the targets file is byte-identical to a run without the
   knob or a book.
4. Tests: `py/tests/research/test_rivals.py` (synthetic leaderboards, price paths and worlds).

## Metric & window
Oct 28 to Nov 2, daily: the number of top-five rivals modeled (win: at least three), their correlation with our
book, and the rivals exposed to our scenario (corr > 0.1 and lift > 1.1). Before enabling the hook: a dry run at
w = 0.5 and w = 1.0 on the VM, comparing the committed scenario and P(M_S > T_S | S) with the live targets.
Post-resolution: each modeled rival's realised balance against its p10 to p90 band (win: inside the band for most
of them), and the realised field maximum against max(T, R).

## Result
Dry run on the VM, 2026-10-25 16:46Z (patched package in a separate read-only container, 50,000 draws, report under
`reports/rivals-run/`; aggregates only):

- Modeled 3 of the top 5 and 13 of the top 20. Two of the top five trade every hour (no quiet window) and one book
  is implausible (gross 1.9x its balance), so they are left out of R.
- Our committed scenario is N9 with red-state-reach, house, midwest-industrial and new-england-incumbency all
  Democratic. All three modeled top-five rivals are exposed to it: correlation with our payout 0.21, 0.23 and
  0.34, lift 1.11 to 1.22 inside the scenario. Over the top 20, 8 of 13 modeled rivals are positively correlated
  with us (2 above 0.3).
- Best modeled rival's resolved multiple R (p10 / p50 / p90): 9.6 / 11.0 / 13.7x with unexplained value held at
  mark, 7.1 / 8.6 / 11.1x with it resolving to zero. The leader's fit leaves half its balance unexplained.
- The tape-fitted T is 16.2 / 21.9 / 35.9x and dominates R in almost every world, so max(T, R) is unchanged at
  16.2 / 21.9 / 35.9x. On this evidence the leaders' marks do not resolve above the tape threshold.
- Our book resolves at 0.86 / 0.93 / 1.29x; P(beat every modeled top-five rival) is 0.000.

## Decision
(pending)
