# EXP-012: Concentrated longshot book, scored on P(1st) against the resolution-adjusted field

- **Proposed:** 2026-10-25 by human (edge hunt: "find every legitimate edge left that raises P(1st)")
- **Approved:** (pending)
- **Status:** draft, not applied (`mode = "off"`)

## Hypothesis
The leaders' 10-11x marks are mostly realised value (longshot-selling bots valued at the Cup's 5 h VWAP mark), so the
bar for first place is about 12-14x in nearly every world. No scenario book within the current caps can reach it
(P(1st) = 0), while nearly all of the balance on one cheap YES leg (ask 0.04 to 0.07) has P(1st) of 2% to 6%. Reporting
that book every run, and committing it if Karthik chooses, raises P(1st) before election night from 0 to at least 2%.

## Change
Branch `exp/012-concentrated-book`. Off by default; nothing trades from it unless `mode = "commit"`.

1. `py/susq/optimizer/concentrate.py` (new): `best_concentrated` scores every verified YES leg with ask <= `max_ask`,
   at each spend share in `fracs` (the rest stays cash), and equal-cost pairs among the best `pair_pool` singles, by
   unconditional `p_first(M, T)` over the simulator's worlds, T per world.
2. `[optimizer.concentrate]` in `config/susq.toml` (`ConcentrateConfig` in `research_env.py`): `mode` off | report |
   commit, `threshold` targets | rivals | max, `max_ask`, `fracs`, `min_gain`. `run_targets` calls the search only when
   mode is not off. `report` appends the best book and its P(1st) to the targets notes. `commit` replaces the scenario
   book's positions (tranche T1, `locked` unless the state reports early) when the concentrated P(1st) beats the
   scenario book's by `min_gain`, and writes scenario `concentrated:<exchange ids>`.
3. `threshold = "rivals"` uses EXP-011's best modeled rival R from `state/research/rivals_book.json`, floored at the
   leaderboard anchor (the leaders that trade every hour have no quiet window, so R alone would leave them out);
   `max` takes the larger of R and the targets T per world. Without a fresh rival book both fall back to T.
4. The rival model gets the Cup's own valuation: `susq research rivals --source vwap5h` builds the price panel from the
   trade tape as the trailing 5 h VWAP (`rivals.tape_vwap_panel`). Against 386 accounts that did not trade for 12
   days it explains hourly value changes with median R^2 0.81, against 0.39 for the book mid and 0.33 for the last
   trade. The default source stays `mid`.
5. Tests: `py/tests/research/test_concentrate.py`.

Committing is only meaningful with the trader's `[risk] per_market_gross_frac` (0.05), the state cluster cap and
EXP-013's sizing raised for the chosen leg; with today's caps the bridge stops buying at 5% of equity.

## Metric & window
Oct 28 to Nov 2, every targets run in report mode: the best concentrated book's P(1st) under `targets`, `rivals` and
`max`, against the scenario book's P(1st) (win for the hypothesis: concentrated >= 0.02 under `max` while the scenario
book stays 0). If committed: shares acquired against target and the average fill versus the ask at decision time.
Post-resolution: whether the leg paid and the realised field maximum against the threshold used.

## Result
Dry run on the VM, 2026-10-27 17:16Z (patched package, scratch state, 30,000 worlds, report mode; aggregates in
`reports/research/edge-hunt-2026-10-25.md`):

| Threshold | Best concentrated book | Best case | P(1st) | Scenario book P(1st) |
|---|---|---|---|---|
| targets (tape T p50 22.0x) | IL Gov R, all-in at 0.040 | 25.0x | 0.0217 | 0.0000 |
| rivals (R p50 12.7x, floor 9.8x) | AL Gov D, all-in at 0.060 | 16.7x | 0.0585 | 0.0000 |
| max | IL Gov R, all-in at 0.040 | 25.0x | 0.0217 | 0.0000 |

Rival fits with `vwap5h` against `mid`: R^2 higher in 16 of the top 19 slots, 4 of the top 5 and 14 of 20 modeled
either way, best modeled rival R 10.5 / 12.7 / 13.6x (mid: 9.8 / 12.4 / 17.8x).

## Decision
(pending: Karthik. Turning it to `report` is free; `commit` trades expected value, about 0.6x to 0.9x of the balance
with a ~95% chance of finishing near zero, for a 2% to 6% chance of first.)
