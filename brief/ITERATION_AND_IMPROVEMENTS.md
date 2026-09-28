# ITERATION_AND_IMPROVEMENTS — Susquatch

SUSQies are play money, so iterate aggressively. Real risk here means disqualification or voided trades, not losses. This file overrides older text in PROMPT.md and SPEC wherever they conflict.

## 1. MVP-first for Oct 1 (only 4 days)

**Must be live by Oct 1, 12:00pm ET (via the Python bridge if Rust isn't ready; see `LANGUAGES_AND_BOUNDARIES.md`):**
- the `SuperMarket` adapter (endpoints verified against the official docs), gateway, OMS, risk, kill switch, and journal;
- `measure_day1`;
- a **tiny first qualifying trade** (1 contract, near the mid, in a liquid market) so I'm ranked from minute one;
- the **constraints racer** (below);
- the **stale-order sniper**.

**Can follow during Oct 1–10:** market making (depending on the decision table), the `sim` C++ core (called from Python, writing target files), Info-tab parsing, and the **cutover from the Python bridge to the Rust live system**.

Everything else follows the SPEC timeline. **Ship the simple version first, then improve it daily.**

## 2. Improvements over the original plan

1. **Constraints racer.** The platform publishes `GET /relationships/constraints` to everyone, and at least one known competitor acts on it every 120–180 s. Poll it every few seconds, within the measured rate limit. Verify each suggested trade against the Info-tab rules (independents, RCV) before acting. Size small, and cancel resting orders when a violation closes. The edge is being faster than the 2–3 minute bots, not the signal itself.
2. **Leaderboard telemetry → empirical tournament model.** Poll `GET /tournaments/{slug}/leaderboard` (top-N balances, `myRank`) hourly and store every snapshot. Use it to:
   - estimate field size;
   - track how fast top balances grow and how dispersed they are (YOLO hits show up as jumps);
   - **replace the prior T(ω) thresholds in `tourney` with fitted ones.**
   Recompute the scenario-book sizing whenever the fitted T moves materially.
3. **Crowd-position inference.** Store every price snapshot. Compare Cup mids to Kalshi/Polymarket mids per market over time. Persistent one-directional drift across a cluster reveals where the field is loaded. That drift feeds the "field least exposed" term in scenario selection.
4. **Tie-breaker hygiene.** Make the final book's exact balance unlikely to match anyone else's: include one idiosyncratic leg and non-round quantities.
5. **Lock-time rule.** Nothing can be adjusted after 12:00pm ET Nov 4. By 11:55 ET every open order must be cancelled and the book must be deliberate. There must be no orphaned resting orders that could fill at the lock and leave an unintended position.
6. **Election-night drill.** Record a full dry run against CA SoS test windows (confirm the dates) and one synthetic replay per week from Oct 12 on. Each run produces `reports/drills/<date>.md` with latency from data arrival to order, false-trigger count, and P&L versus the replay truth.

## 3. Iteration loop

- **Daily ops report** (`reports/daily/<date>.md`, auto-generated at 7:00 PT):
  - P&L by strategy;
  - fills;
  - constraints-racer hit rate and latency;
  - Sponsor spread stats;
  - leaderboard position and fitted T;
  - current P(1st) estimate from `tourney`;
  - risk events;
  - the **top 3 proposed changes**.
  I reply with which to apply.
- **Experiment registry:** `experiments/EXP-*.md` (hypothesis, change, metric, result, decision). Changes can ship daily, but each one gets logged so the post-contest write-up can attribute results honestly.
- **Weekly strategic review** (Sundays): re-run the scenario selection with fitted field data, review the decision table against observations, and adjust the phase timing.
