# EXP-002: Fund the early scenario book from the far-quote reserve, not the sniper book

- **Proposed:** 2026-10-05 by weekly-strategy
- **Approved:** (pending) by Karthik
- **Status:** proposed

## Hypothesis
With `early_book_frac = 0.20` and `farquote reserve_frac = 0.08`, the scenario executor buys the full early book from
free cash between Oct 7 and Oct 14 without any cash-driven risk refusal, and the far quoter's fill count (0 in 5 days)
does not change.

## Change
`config/susq.toml`: `[optimizer] early_book_frac = 0.20`; `[strategies.farquote] reserve_frac = 0.08`. The 30% book was
unfundable: free cash is about 13,500 after the 13,755 far-quote reserve, against a 30,942 book. Selling sniper positions
instead would cost about 7 pts of expected value per share.

## Metric & window
Oct 7 to Oct 14: scenario legs bought as a share of target quantity (win: 100%), count of `risk_rejected` reasons
mentioning cash or budget on scenario intents (win: 0), far-quote fills (expect 0, any fill is upside).

## Result
(pending)

## Decision
(pending)
