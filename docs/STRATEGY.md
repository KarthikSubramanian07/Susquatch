# Strategy to finish first (written 2026-10-06, day 6 of 34)

This is the operating plan from here to the lock. It replaces the loose reading of SPEC §3 that we ran
on for the first week. Inputs: the Cup rules, the live field data (aggregates only), the two research
memos under `reports/research/` (literature review; how winners of comparable contests won), and the
election-night readiness audit. Experiments that implement it are in `experiments/`.

## 1. The scoring rule, read correctly

- Final rank = verified balance **after every market resolves** (rules; docs/API.md). The live
  leaderboard is a mark. Trading closes 2026-11-04 17:00Z, after election night, before most calls.
- Prizes $30k / $5k / $2.5k. Second is worth a sixth of first. The objective is P(rank 1 at
  resolution), nothing else. Expected wealth and Kelly are the wrong objectives (Seel & Strack 2013;
  Fang, Noe & Strack 2020; Haugh & Singal 2021).
- Ties split prizes. Everyone starts at exactly 100,000, so the book always carries one idiosyncratic
  leg and non-round quantities.
- The sponsor may re-resolve, void, or adjust anything. Capital spent on marks has zero value and
  carries disqualification risk. **No order is ever placed for its effect on the mark.**

## 2. What the field looks like (Oct 6, aggregates)

| Fact | Value | Source |
|---|---|---|
| Members / ranked | 4,262 / 1,263 | leaderboard |
| Accounts that turned over ≥ 100% of balance | 496 | leaderboard volume |
| Marked ≥ 3x / ≤ 0.5x | 12 / 65 | leaderboard |
| Top mark | 9.9x, flat since Oct 4 | leaderboard |
| Cup vs fair, legs with fair < 0.10 | +4.0 pts rich | fair_live vs cup mids |
| Cup vs fair, legs with fair ≥ 0.90 | −3.7 pts cheap | same |
| Cup vs real money, Democratic direction | about −1 pt (field leans R) | telemetry drift |

The top marks come from whole balances bought at 0.95 on ~1% outcomes; those resolve to ~0. The
field as a whole overpays for longshots and underpays for favourites, exactly as the CSPI/Salem data
and the 292M-trade calibration study predict. Three public bots run cross-venue lag arbitrage against
the Cup, so that edge is a floor (1.1 to 1.5x), not a path to first.

## 3. The three layers

### Layer A, carry (Phase 1, running): the floor, sized as insurance
Sniper and far quoter buy near-certain outcomes the field sells cheap. Expected finish 1.05 to 1.15x.
It funds nothing that wins; it keeps us above the ~65% of the field that ends below 1x. Budget stays
at the current caps (sniper 35% gross, far quoter 8% reserve). Review weekly; cut if realised
multiple is under 1.05x by Oct 19.

### Layer B, the scenario book (Phase 2, buying from Oct 7)
Concentrated cheap legs that all pay under one election scenario, chosen to maximise
P(S) · P(M_S > T_S | S). Changes from the first week:

1. **Threshold from positioning, not marks.** EXP-001 (shipped) removes the top-1 growth artefact
   (T50 93x to 15x). EXP-004 replaces the level anchor with a field model fitted to the trade tape:
   cumulative volume at each price per contract gives the distribution of resolved multiples the
   field can reach in each world, and the leaderboard's volume column gives how many accounts are
   all-in. Re-fit every Sunday.
2. **Robustness over three field assumptions.** Rank scenarios under T = thin (1.5x), medium (5x),
   and tail (Seel–Strack, 20x). Commit only to a scenario that ranks top under all three; if the
   ranking flips, buy the robust half and wait for more tape.
3. **Regional, not national, scenarios.** The grid is already national decile × four cluster signs.
   EXP-005 adds the 2018 pattern (Midwest/FL R-miss with Southwest D-miss) and the 2022 pattern
   (near-zero bias, markets too R) as explicit cells with Shirani-Mehr magnitudes (shared 2 pts,
   RMSE 3.5).
4. **Price every cheap leg against the longshot bias.** A leg at 0.05 to 0.15 is worth ~0.75 of face
   in isolation; it enters the book only if its probability inside the scenario clears the ask by that
   margin. Legs that clear unconditionally are Layer A trades and are sized there.
5. **The 2022-prior detector.** Weekly: Cup Senate-control and key-race prices versus Silver
   Bulletin, DDHQ and FiftyPlusOne. If the Cup is more Republican than the models while the generic
   ballot is D+6 or better, "polls right, Democrats outperform the Cup in the Senate" is the lead
   contrarian scenario.
6. **Decorrelate from the field.** The crowding term already inflates T_S by field exposure; with the
   tape it becomes a per-leg term: avoid legs where the cheap-side volume is concentrated.

Sizing stays 20% of equity until Oct 21 and 60% after, per SPEC. Legs in evidently manipulated thin
markets (price far from the real-money reference with tiny depth) are excluded unless the scenario
requires them, with a written rationale.

### Layer C, election night (Phase 3, Nov 3 19:00 ET to Nov 4 11:55 ET)
This is where first place is decided, because the lock lands 16 hours after polls close and the
decisive races (AZ, NV, CA, WA House seats, Maine RCV) will still be open. Two mechanisms:

1. **Count-order playbook (EXP-006).** Pre-register per state the direction and size of the
   partial-count bias by hour from the 2022 and 2024 timelines (PA mail-first then red shift; Maricopa
   late-early ballots R; Nevada mail D for four days; CA weeks). Trade only when the Cup price moves
   against the pre-registered direction by more than the modelled uncertainty.
2. **Needle-driven rolling.** The county model (`needle`) updates race posteriors from results feeds;
   the executor buys where q − ask clears the threshold and rolls appreciated legs into later-closing
   races. Leader/contender switch (Staněk): once the posterior says the Layer B book has paid and we
   are above the modelled field maximum, hedge toward consensus on open races; otherwise roll into the
   next cheap correlated scenario the count has just made cheaper.

Readiness is the gating risk. The audit of Oct 6 (`reports/research/election-night-readiness-2026-10-06.md`)
found: the Python bridge that trades today has no election-night code at all; the only capable path is
`rust-live --election` with the C++ `needle` and NATS, which has never run as a whole; there is no 2026
needle prior; `[strategies.en]` is absent from config so the executor is disabled; posteriors expire
10 s after each batch; results feeds cover CA and GA only (5 of 117 mapped races); rust-live has not
had a paper run since Oct 3; the cutover would cancel the bridge's resting ladders; and the compose
memory ceilings sum to 6.5 GB on a 4 GB host. The minimum viable path (CA+GA, about 60 to 70
engineering hours) is: config, timeout and glitch-cap fixes Oct 7 to 8; images shipped Oct 8 to 9;
a 72 h rust-live paper run Oct 9 to 12; the 2026 prior Oct 12 to 15; the cutover runbook Oct 15 to
17; rehearsals on the CA SoS test days Oct 21, 28 and 31. The full path adds SC, IA, NC and two
custom-site states (110 to 150 hours). Everything in Layer C is conditional on this schedule holding.

## 4. Endgame rules (from the Salem post-mortems)
- From Oct 28, model the top five rivals' likely payout vectors from the tape and avoid positive
  correlation with them on the markets where they are overexposed.
- Keep a cash floor that locks a placing before the last roll; size the final trades so the finish is
  the same either way a swing market resolves.
- Read every resolution rule like a lawyer: chamber control settles manually; "N/A" refunds.

## 5. Calendar
| Date | Action |
|---|---|
| Oct 7 | Scenario book starts buying (20%). Tape recorder live. |
| Oct 11 (Sun) | Weekly review: tape-fitted T, three-field robustness, 2022-prior detector. |
| Oct 18 (Sun) | Weekly review; Layer A keep/cut decision. Election-night minimum path code complete. |
| Oct 21 | Book to 60%. |
| Oct 25 (Sun) | Weekly review; full election-night rehearsal (replay + chaos). |
| Oct 28 | Endgame rival model; sniper unwind decision (EXP-003). |
| Nov 1 (Sun) | Final review; needle prior staged; profiles switched; dashboard drill. |
| Nov 3 19:00 ET | Election night engine. |
| Nov 4 16:55Z | Hard stop. |

## 6. What this plan will not do
It will not chase marks, wash trade, coordinate with other accounts, or run anything the sponsor could
read as manipulation. It will not pretend the field maximum is known: every targets file states the
field assumption its scenario was chosen under.
