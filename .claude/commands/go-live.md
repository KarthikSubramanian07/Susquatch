---
description: Pre-flight checklist, then arm live trading (only when Karthik explicitly says "go live")
---

Karthik has asked to arm live trading. **Stop immediately unless his message in this session literally says "go live".**

Pre-flight (all must pass; show a table of the results):
1. `.env` has `SUSQ_API_KEY` set. Show only its length, never its value.
2. `uv run susq-bridge measure-day1 --read-only` succeeds: host reachable, tournament resolved, `isPendingEnrolment` false,
   and no `TERMS_NOT_ACKNOWLEDGED`.
3. The lease is free or held by the process being armed. `ops/KILL` is absent.
4. `state/market_map.json` exists. Count the rows with `verified: true`. The sniper trades only those.
5. The current time is before `calendar.last_order_utc` in `config/susq.toml`.
6. `make test` passes on this checkout.

If everything passes:
- Write `ops/GO_LIVE` containing `go live — armed by Karthik <ISO timestamp>`.
- Set `SUSQ_MODE=live` in `.env`, keeping everything else unchanged.
- Log a line in `research_log.md`, with no Competition Data.
- Tell Karthik exactly what will now trade (first qualifying trade, racer, sniper) and how to disarm: delete `ops/GO_LIVE`,
  set `SUSQ_MODE=paper`, or press Kill on the dashboard.
