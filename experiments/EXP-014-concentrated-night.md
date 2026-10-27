# EXP-014: A pre-registered concentrated election-night profile ("contender mode")

- **Proposed:** 2026-10-25 by weekly-strategy (reports/reviews/EXP-draft-concentrated-night.md)
- **Approved:** pending. Arming it is a risk decision only Karthik can make.
- **Status:** built, off by default; arming decided at the Nov 3 pre-flight after rehearsal #2

## Hypothesis
Under today's config, P(1st) is zero.
- The pre-election book pays at most 5.4x, against a tape floor of 9.67x and a T50 of 22.0x.
- The election-night executor can have at most 25% of equity deployed (`night_budget_frac`), 2% per order and 5%
  per market. It rolls once the bid comes within 2 points of the posterior, after an edge of at least 8 points.
  That captures about 6 points per cycle, which grows equity about 3% a cycle. T50 would take about 105 cycles.
- Reaching T50 from 0.98x needs about 4.5 successive doublings in one night, with the last roll about 1.1M of
  notional (weekly review 2026-10-25, section 5).

A profile that concentrates the stake on the needle's highest-edge races in the deepest markets, holds each position
until the race is called, and chains the proceeds into the next race is the only path with a nonzero P(1st).
Falsifiable test before the night: replayed on the 2022 timeline (rehearsal #2), the profile reaches 10x or more in
at least one of N seeded replays without breaking any risk invariant other than the ones it deliberately relaxes,
and it never trades on a stale or invalid needle.

## Change
Code is built and merged off. Nothing changes on the night unless `contender_mode = true` is set at the Nov 3
pre-flight. All knobs live in `config/susq.toml [strategies.en]`; with `contender_mode = false` every other
`contender_*` key is inert (tested: decisions are identical whatever their values).

| Knob | Default | What it does when `contender_mode = true` |
|---|---|---|
| `contender_mode` | `false` | Turns the profile on. |
| `contender_budget_frac` | 0.80 | Night budget for needle-derived orders, instead of `night_budget_frac` (0.25). |
| `contender_exit_bid` | 0.93 | Hold to the call: a held leg is sold once its bid is at least this, at no less than it. The 2-point roll is not taken. |
| `contender_call_se` | 0.005 | The needle has called the race once the model SE of q is at or below this; the leg is then sold at no less than q − `min_edge_pts` (a leg called against us is dumped at any bid). |
| `contender_market_frac` | 0.30 | Per-market cap for needle-derived buys, and the cap of that market's single-race `state` / `house` cluster, while the book is deep. Region and national caps stay. |
| `contender_min_depth` | 100000 | SUSQies of notional the book must hold near the touch on the side taken, read by risk off the fresh book when the order is checked. Below it the 5% cap stands. |
| `contender_depth_window_pts` | 3.0 | How far from the touch that depth counts. |
| `contender_loss_per_hour_frac` | 0.50 | Loss-per-hour breaker threshold instead of `[risk] loss_per_hour_frac` (0.03). |

Chain: buys still clear the unchanged `min_edge_pts` (8) and `se_mult` (3), the count-order gate and corroboration;
in contender mode they are collected across all races and sent ranked by edge × notional available within the limit,
so the budget freed by each exit goes to the best next race first.

Unchanged and tested in contender mode: kill switch, needle watchdog (no orders while the needle is stale or down,
working orders cancelled), stale-feed and dispute gates, glitch cap, corroboration, `max_order_frac` (2% per order),
order size never larger than the depth within the limit, locked legs never sold.

Code: `crates/core/src/config.rs` (`EnCfg`), `crates/strategies/src/en_executor.rs` (exit, ranking),
`crates/risk/src/limits.rs` (`ContenderCap`, `DepthProbe`), `crates/risk/src/breakers.rs`
(`BreakerConfig::with_contender`), `crates/book/src/lib.rs` (`L2Book::depth_near_touch`), `crates/live/src/engine.rs`
(wiring), `py/susq/replay/contender.py` (drill).

Cash source: the EXP-003 unwind (about 45 to 55% cash expected by Nov 3) plus selling near-certain holdings on the
night at the bid.

## Known limits (read before arming)
- **The breaker raise is account-wide.** The loss-per-hour breaker sees only account equity, so it cannot exempt
  needle-derived positions alone; with the mode on, every strategy's losses are judged at 50% an hour.
- **The state / house cluster is lifted with the market.** Without it a Senate or Governor market would stop at the
  20% state cluster cap, so the 30% knob would be dead outside chamber-control markets.
- **2% per order still binds.** A 30% position takes at least 15 filled orders; rust-live re-decides every tick, so
  this costs seconds to minutes, not the trade. Rehearsal #2 should measure how long a full build takes.
- **The drill's market is our own posterior, delayed.** It shows the shape of what the rules compound to, not the
  needle's real edge over the field. On the bundled 3-race 2022 sample (50 seeds, 2026-10-26): contender p50 1.32x,
  max 1.52x, 0 of 50 runs at 10x; default p50 1.05x. Three races cannot chain to 10x; the 71-race 2026 spec is the
  meaningful run.

## Metric & window
- **Rehearsal #2 (Oct 29 to 30):** the replay multiple distribution, the number of invariant breaches (win: 0 on the
  unchanged invariants), orders placed while the needle was down (win: 0), and any order larger than the measured
  depth (win: none).
- **On the night:** the final verified balance. P(1st) is the only objective.

Do not enable it on the night if rehearsal #2 shows any order on a stale or invalid needle, or any roll larger than
the measured depth.

### Extra flags for rehearsal #2 (do not change ops/rehearsal.sh or RUNBOOK §2b from this PR)
1. In the VM checkout's `config/susq.toml`, beside `[strategies.en] enabled = true`, set `contender_mode = true`
   (leave the other `contender_*` values at their defaults). Revert with `git checkout config/susq.toml` afterwards,
   as for `enabled`. rust-live logs `election-night contender mode is ON` at start; check for it.
2. Offline distribution (any machine, no NATS):
   `uv run python -m susq.replay drill-contender py/susq/replay/specs/general_2026.toml --backend cpp --seeds 200`
   (add `--chaos-drop 1800:600 --chaos-malformed-every 50 --chaos-kill 5400:120` to match the bus chaos, and
   `--start-multiple <equity/100k at the time>`). It writes `reports/drills/<date>-contender.md` with p10/p50/p90,
   runs at or above 10x, the best path with depth per roll, refusals while the input was down, and max order /
   depth (must stay at or below 1.00).
3. Record from the paper run: every `en_order` event with `"contender": true` (`exit`, `deep_book`,
   `depth_near_touch`, `score`), every `risk: per-market cap` and `election-night budget` refusal, and the paper
   equity path.

## Result

## Decision
