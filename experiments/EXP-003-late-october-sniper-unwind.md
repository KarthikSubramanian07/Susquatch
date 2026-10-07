# EXP-003: Unwind the sniper book in the last week of October

- **Proposed:** 2026-10-05 by weekly-strategy
- **Approved:** 2026-10-07 by Karthik
- **Status:** shipped

## Hypothesis
From Oct 24, selling sniper positions whose Cup bid is within 2 pts of fair converts the 72%-of-equity sniper book into
cash at an average haircut of 2 pts or less, which funds the full-size scenario book (Oct 21 onward) and the election-night
reserve without selling at today's 7-pt discounts.

## Change
Shipped in both traders with identical rules: the bridge (`bridge_py/susq_bridge/strategies/sniper.py`, `plan_unwind`),
which trades until the Nov 2 cutover, and rust-live (`crates/strategies/src/sniper.rs`), which trades on election night.
A shared fixture (`tests/fixtures/sniper_unwind.json`) is run through both and must produce the same sells.

Config, `[strategies.sniper]` (the shipped values equal the defaults in both traders):

| Key | Value | Meaning |
|---|---|---|
| `unwind_from_utc` | `2026-10-24T16:00:00Z` | from here the sniper buys nothing and only unwinds |
| `unwind_max_haircut_pts` | 2.0 | sell at max(bid, fair − 2 pts), never below that floor |
| `unwind_target_cash_frac` | 0.60 | stop once cash is at least 60% of equity |
| `unwind_max_hourly_frac` | 0.10 | unwind proceeds sent in any trailing hour stay under 10% of equity |

**The unwind set.** The platform reports positions per account, not per strategy, and rust-live never saw the bridge's
fills, so neither trader can tell which holdings the sniper bought. The unwind set is therefore a rule. A held side is in
it when all of these hold:

1. the market is verified, and its fair is fresh and within the mapping-error gate (the same gate a snipe passes);
2. the held side's fair is at least 0.90 (YES held with fair ≥ 0.90, or NO held with fair ≤ 0.10), which is the
   sniper's near-certain book and excludes the scenario book's cheap legs;
3. the exchange is not a locked leg in the current targets file;
4. no other strategy has an order working on the exchange (orders of unknown origin count as another strategy), and the
   sniper has none working there either.
5. On election night, rust-live only: the needle has no posterior for the market's race. Those races belong to the
   election-night executor, and its filled buys must not be sold back to top up cash. The bridge has no needle, so
   before the night the rule is the same in both traders.

Far-quote fills on near-certain sides qualify too, but only once no far-quote ladder is resting on that exchange.

**Each sell.** It is covered: never more than the holding, so the position never flips short. Its price, in the held
side's terms, is max(best bid, fair − 2 pts rounded up to the 0.005 tick, toward fair). It never goes below that floor:
when the bid is deeper, the order rests at the floor for `order_ttl_s`. Its size is the smallest of four limits: the
holding; the existing per-order rule (proceeds at most `max_mistake_frac` = 1% of equity); the cash still missing to
the 60% target; and what is left of the hourly proceeds cap. Exchanges are taken in id order. The pass stops at the
cash target or at the hourly cap. It waits while cash is unknown, and in the bridge it also waits while the targets file
cannot be read and no earlier read succeeded. Stale-fair cancels and the gateway's risk checks apply as before. A
locked leg is refused both by the sniper and, in rust-live, by risk.

**Journal.** Every sell is journaled as `sniper_unwind`, with the exchange, side, quantity, limit, held-side fair, floor,
bid, haircut in pips and (bridge) the gateway status. The bridge also journals `sniper_unwind_state` when the reason
for pausing changes: `cash_target`, `hourly_cap`, `no_cash` or `targets_unreadable`.

## Metric & window
Oct 24 to Nov 2: realized haircut per share sold (win: mean 2 pts or less), cash on Nov 3 12:00 ET as a share of equity
(win: 60% or more), no sale below fair - 2 pts.

## Result
(pending)

## Decision
(pending)
