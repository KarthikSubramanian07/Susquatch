# EXP-008: Scenario book on the bridge

- **Proposed:** 2026-10-11 by weekly-strategy
- **Approved:** 2026-10-11 by Karthik
- **Status:** running

## Hypothesis
The live account holds no scenario legs until the Nov 2 cutover because only rust-live has a scenario executor. A bridge
port bought from 2026-10-11T16:00Z reaches the 20% early book by Oct 21 at an average entry within 1 pt of the ask
the Rust paper run gets.

## Change
`bridge_py/susq_bridge/strategies/scenario.py` ports `crates/strategies/src/scenario_executor.rs` exactly. It covers
tranches of `tranche_frac` of the gap, `min_tranche_qty`, `start_utc`, `min_interval_s`, `max_hourly_notional_frac`,
the YES-space `max_entry_price` cap, verified markets only, fresh books only, no reduction of locked legs, and no
increase on stale targets. It reads the same `[strategies.scenario]` section, and a shared fixture
`tests/fixtures/scenario_tranches.json` is asserted by both test suites. The bridge's sniper already skips locked
legs, so the two cannot trade against each other.

Both executors also clamp each tranche to the `[risk] max_order_notional_frac` per-order cap (rust-live's paper run
logged `order notional 2500.05 > 545.20` on Oct 11: a tranche over the cap was refused on every decision, so that
leg was never bought).

## Metric & window
Oct 11 to Oct 21: live scenario-leg notional against the targets' early-book cost (21,073 at seq 54), and the mean
entry price minus the ask at decision time. A win means at least 80% of the book is filled with mean slippage under
1 pt. Revert if any tranche trades above `max_entry_price`.

## Result

## Decision
