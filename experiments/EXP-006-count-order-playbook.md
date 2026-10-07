# EXP-006: Pre-registered count-order playbook for election night

- **Proposed:** 2026-10-06 by weekly-strategy (literature review and winners memo 2026-10-06)
- **Approved:** 2026-10-06 by Karthik
- **Status:** shipped (branch `exp/006-count-order-playbook`)

## Hypothesis
The Cup's election-night prices extrapolate partial counts the way 2020 to 2024 markets did (Pennsylvania mail-first
then red shift; Maricopa late-early ballots Republican; Nevada late mail Democratic; California weeks), so a table
of pre-registered hour-by-hour partial-count biases per state, traded only when the Cup price moves against the
registered direction by more than the modelled uncertainty, beats the needle's unconditional threshold rule on a
2022 and 2024 replay.

## Change (final)
1. **`config/count_order.toml`, registered 2026-10-06.**
   - **Scope.** 18 states (AK, AZ, CA, GA, IA, KS, ME, MI, NC, NE, NH, NV, OH, PA, SC, TX, WA, WI) from the two research memos
     (`reports/research/count-order-and-detector-2026-10-06.md`, `count-order-IA-GA-WI-2026-10-06.md`).
   - **Each row.** Poll close (UTC, consistent with `config/poll_closing.toml`), drift and band at +1, +3, +6, +12 h and the lock,
     `final_h`, vote types in reporting order, share counted at the lock, notes, and the memo section.
   - **Overrides.** CA House seats, WA-03, AZ governor and NE-02.
   - **Bands.** One rule, written before any replay: half the memo range, plus an allowance of 0.5 pt per estimate, 1.0 pt for an
     estimated first-hour value of 5 pts or more, and 0.25 pt for an "about" print.
   - **Direction.** A drift has a direction only when `|drift| > band`.
   - **Parsers.** `crates/core/src/count_order.rs`, which validates the file, and `py/susq/replay/count_order.py`.
2. **`crates/strategies/src/en_executor.rs`: the count-order gate** (`[strategies.en] count_order_enabled = true`).
   - **No adjustment.** The needle's `margin_mean` already projects the remaining vote: `cpp/needle` adds each county's expected
     remaining votes at the prior's vote-type offsets. The table is therefore **not** subtracted from the margin, which would count the
     lean twice. It is a direction check and a band.
   - **The rule.** A buy that agrees with the registered direction of the count still to come needs the normal threshold. A buy against
     it, or in a race with no direction, needs the threshold plus the band converted to probability through the posterior's margin and
     sd. Without a margin sd, a gated buy is refused.
   - **Rolls** are never gated.
   - **Journal.** Every order is journalled as `en_order` with the registered profile, drift, band, direction and the band paid.
3. **Lock-time rule** (`lock_rule_enabled = true`).
   - **Window.** From `lock_window_start = 2026-11-04T14:00:00Z`.
   - **Eligible races.** Counted below `lock_max_reported_frac = 0.85`, with registered lock drift of at least
     `lock_min_drift_pts = 1.5` and a band smaller than the drift. On the shipped table that is CA, CA House seats, NV and PA.
   - **The buy.** These races may be bought on the favoured side at the normal threshold, on the needle's last posterior even after
     `[needle] posterior_max_age_s`.
   - **Safety.** The needle must be healthy and the posterior no older than `lock_posterior_max_age_s = 10800`. Disputed and
     stale-feed races are skipped, and quiet posteriors are never rolled on. Sizing is unchanged (per-order cap, night budget, risk).
   - **Plumbing.** The posteriors arrive through `NeedleSnapshot::quiet_posteriors` (crates/bus) and `NeedleView.quiet_posteriors`.
4. **Leader/contender switch, minimal** (`contender_hedge_multiple = 0.0`, off).
   - When equity ÷ 100,000 reaches the multiple, the executor opens no new positions and only rolls.
   - It is off because the field maximum on the night is not observed, so no honest multiple exists to switch at.
   - The spec's "hedge toward consensus" is not built; stopping new exposure is the safe subset.
5. **Drill.** `uv run python -m susq.replay drill-compare <spec>` replays once and runs the drill executor with the gate off and on.
   It writes `reports/drills/<date>-count-order.md`.

## Metric & window
Before Nov 3: replay P&L multiple of gate-on versus gate-off on both years (win: gate-on higher on both, with fewer
reversals). Nov 3 to 4: realised fills by hour and the posterior at lock, reviewed after resolution.

## Result
Pre-election drill, 2026-10-07, bundled 2022 sample (`py/susq/replay/specs/sample_2022.toml`: GA Senate, NH Senate,
ME Governor; real county totals, synthetic timing; reference needle backend):

| Market lag | Gate off | Gate on | Refused by the gate |
|---|---|---|---|
| 600 s | 4 trades, +91.5 | 2 trades, +55.7 | GA DEM (+49.0 off) and GA REP (−13.2 off), both in Georgia's first hour (−4.5 ± 17.5, no direction) |
| 900 s | 3 trades, +104.7 | 2 trades, +55.7 | GA, first hour |

The win condition is not met on this fixture, and the fixture cannot meet it.

- **No mispricing to exploit.** The drill's market is our own posterior delayed, never a book that extrapolates the partial count.
  The gate can only refuse trades there, and here it refused one winner and one loser in Georgia's first hour.
- **No 2024 data.** There is no 2024 fixture, so the two-year comparison in the metric was not possible with what is in the repo.
- **What a real test needs.** Timestamped Cup or Kalshi prices from a past night, set against county timelines.

Nov 3 to 4: pending.

## Decision
(pending: review after resolution, with realised fills by hour, lock-rule fills and the posterior at lock)
