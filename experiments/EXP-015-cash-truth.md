# EXP-015: Report true cash and cap use, and seed the paper run from it

- **Proposed:** 2026-10-25 by weekly-strategy
- **Approved:** 2026-10-25 by Karthik
- **Status:** running

## Hypothesis
`state/status.json` `cash` (and therefore the dashboard and the daily packets) shows `bridge.tournament.my_balance`
from bridge start-up (`bridge_py/susq_bridge/status.py`). That value is 27,254.41, unchanged since Oct 7. The cash
the bridge's risk and the EXP-003 unwind actually use is `totalAccountValue - totalHoldingsValue`
(`gateway.portfolio`): 17,305 at 2026-10-25 11:27Z and 21,163 at 15:04Z. The rust-live paper run is seeded from the
same stale `myBalance`, so it trades with about 6 to 10k more cash than the live account will have at the cutover.
Separately, the scenario book was refused on caps for six days (Oct 19 to 25) without any daily review flagging it,
because the packet has no cap-use figures. Reporting true cash and cap use will make the cutover comparison
(RUNBOOK §2a) and the daily reviews accurate.

## Change
- `bridge_py/susq_bridge/status.py`: `cash` comes from the last portfolio read (`risk.cash`, which `Gateway.portfolio`
  sets to account value minus holdings). It falls back to `my_balance` only before the first read. The live bridge is
  not restarted mid-run, so this takes effect only if the bridge restarts.
- `bridge_py/susq_bridge/runtime.py`: a paper bridge seeds from account value minus holdings, not `myBalance`.
- rust-live (`crates/live/src/cash.rs`): the paper run's seed and the engine's `world.cash` (live and paper) come from
  the portfolio P&L read, with `myBalance` only before the first read. `world.cash` feeds rust-live's status cash,
  the `susq_cash` gauge and the buying-power check, so the dashboard after the Nov 2 cutover shows true cash.
- Daily packet `metrics.csv` (`py/susq/reports/capuse.py`): `cash`, `cash_frac`, `total_gross_frac`, `cluster_1..3`
  with `cluster_n_cap_use` (used over the tightest of the `[risk.clusters]` kind cap and the targets file's override),
  `scenario_orders` and `scenario_refused_caps`. All are recomputed from the day's last pnl, positions and complete
  open-orders reads in the live trader's journal. A new `scenario-caps` rule proposes a change when more than half of
  the day's scenario orders were refused on caps.
- `/daily-review`: flag any day on which over 50% of scenario orders were refused on caps.

## Metric & window
From deploy:
- `status.json` cash is within 1 SUSQie of the bridge's `sniper_unwind_state.cash` on every check (after a restart of
  whichever process writes it).
- The paper run's starting cash after the next restart is within 1% of live cash.
- The first packet after deploy reports cap use. Its `total_gross_frac` matches an independent recomputation from the
  positions and open-orders reads within 0.01.

## Result

## Decision
