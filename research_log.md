# Research log

Timestamped, append-only. A scrubbed copy of this file feeds the write-up, so it contains **no raw Competition Data**:
no order IDs, fills, or other players' names. Aggregates are fine.

## 2026-09-28

- **16:45 PT.** Read the brief end to end. Pulled the official API docs: `sig.thesuper.market/api/v1/docs` is a Scalar page with the whole
  OpenAPI 3.1 document inlined, so `scripts/fetch_api_docs.py` extracts it verbatim. It has 102 operations, 67 of them
  DMM/admin-only. The unofficial notes were close on shape but wrong on tick size (0.005, not 0.01), and they missed realtime,
  GTD expirations, batch/multi-leg orders, server-side STP, and the ALL collateral model. All of these are logged in `brief/ERRATA.md`.
- **16:50 PT.** Both `www.` and `sig.` hosts return the documented error envelope unauthenticated (Vercel, `x-request-id`).
  No rate-limit headers before auth.
- **Design consequence.** Realtime `relationships:violations:{tournamentId}` pushes violations in about 250 ms, while known competitor
  bots poll every 120–180 s. The constraints racer should be event-driven. GTD expirations give a dead-man switch for free,
  so every order carries one.

## 2026-09-29

- **Build day, in parallel.** Six components built on separate branches, each merged through a PR with CI:
  platform + mock, bridge, rust-live, C++ models, research, election night, ops + dashboard. All 16 PRs merged.
  Integrated `main` runs 1,178 tests green: Python 609, Rust 416, C++ 101, dashboard/Worker 52.
- **Calibration sanity check.** Calibrating race marginals to public Kalshi/Polymarket mids gives **59.2–59.5 %** for Senate
  control from correlation alone, against **62 %** on the chamber market. That's inside the SPEC's 2–4 point band, untuned.
- **Interop run found a real bug.** The release `susq-live` binary against the Python `susq-mock`, two independent implementations of the
  same docs, worked end to end: tournament resolution, 4 realtime channels, violation detected within 1 s, time gate respected pre-open.
  With the open moved to the past, the racer fired **16 bundles in 20 s** on one standing violation, because paper fills don't consume liquidity.
  Both racers now have a re-fire cooldown and a per-relationship exposure cap (#12). Lesson: a guaranteed payoff isn't a reason to trade without limit.
- **Data-source corrections:** CA SoS test dates (E18), Georgia's move off Clarity (E19), a Kalshi ticker/title mismatch (E20).
- **Needle.** Interval coverage is nominal over 300 synthetic mail-first nights, and 3,000 counties recompute in 0.8 ms.
  Confirmed count corrections now flow from enr to needle (#14) instead of being dropped as regressions.
- **CA test windows resolved.** The brief's Oct 22/29/Nov 1 are the 2025 special election's dates (schedule v19). Schedules v17–v20 all use
  Wed E−13, Wed E−6, Sat E−3, which puts 2026 at **Oct 21, Oct 28, Oct 31**, 8–12 PT. A systemd timer now captures them (#18).
- **Georgia** slugs are ad hoc (`GeneralPrimary51926`), so the feed now discovers the Nov 3 election by date and its ballot items by name.
  Verified live on the May primary: 1,236 county results matching official splits (#20). Test payloads are withheld.
- **ALL collateral** is modeled everywhere, but live sizing stays on full notional until day 1 shows the engine matches the model (#21).
  Against the mock, the racer's bundle grows from 52 to 123 shares with collateral on, so this is worth getting right.
- **Dashboard live** at susquatch.pages.dev, showing the real pre-open state. Lesson learned while deploying: never `source` a `.env` in a shell,
  because an unquoted `a|b|batch` value runs `batch` (#19).

## 2026-09-30

- **Armed for live trading** by Karthik at 10:59 UTC, after a clean pre-flight (probe, lease, kill switch, calendar, tests).
  Only human-verified two-party races trade at the open; races with RCV, runoffs, independents or chamber control wait for an Info-tab read.
- **Day-one fixes** (#25): the map drafter could not read the recorder's real files, so no market would have been tradable;
  the Mac startup script resolved the wrong folder; a race with a listed Independent market now flags every leg.
- **Moved to a server** (#27, #28): Azure for Students (Arm, 2 vCPU / 4 GB, credit-funded, no card) with nightly Backblaze B2 backups.
  Oracle and R2 were dropped because both need a card. The stack survived killed containers and a full reboot, coming back armed.
- **All 237 markets approved** on Karthik's instruction (RCV, one-venue, independent, runoff and chamber-control races).
  The Cup's settlement guide settles runoffs and chamber control by administrators once the result is official, and final
  standings wait for every market, so the Cup resolves on the same outcome as Kalshi and Polymarket. The racer still skips
  any race where an independent can win. Every market has at least one reference price.
- **Collateral sizing is now `"auto"`**: it switches on by itself the moment the day-one probe shows the engine matches the model,
  persists across restarts, and turns back off on a measured mismatch. Before, it waited for a human to read the report.

## 2026-10-01

- **The open was noon ET (16:00Z), not midnight.** The bridge slept to the right instant and ran the day-one checks on time.
- **Lease lapse at the open.** The trader lease has a 30 s TTL, and renewal only began after the slow day-one checks, so the
  lease expired while they ran and nothing could trade. Fixed in [#30](https://github.com/KarthikSubramanian07/Susquatch/pull/30):
  renewal starts at acquire, and a lapsed lease is retaken instead of leaving the process idle.
- **`TERMS_NOT_ACKNOWLEDGED`** blocked every write until Karthik accepted the rules on the website at about 17:30Z. The
  summary flagged it as ACTION NEEDED, which is what it was built for.
- **Platform overload.** The first hour returned 500s and timeouts on most endpoints. [#31](https://github.com/KarthikSubramanian07/Susquatch/pull/31)
  made the day-one steps best effort (a failed check no longer blocks trading), stopped `measure-day1` from re-running on every
  restart, retried startup reads, and retried the first trade under a fresh intent.
- **First trade filled at 17:36Z**, one share, which earns us a leaderboard rank. The fill cost exactly price × quantity: no fee.
- **Day-one census** (237 markets): 98% two-sided, median spread 7 ticks, median top-of-book depth 200 shares. **No sniper edge**
  at or above the 3.5-point threshold anywhere; the best gap was 2.3 points. Cup prices sit within about 2 points of Kalshi/Polymarket
  fair, as the Sept 27 research predicted. The Sponsor is a sharp, tight counterparty from minute one.
- **Rules and platform research.** The rules were revised Sept 30 (eligibility narrowed, "up to" prizes, common-control accounts)
  and the Oct 1 changelog changed two things we had designed around: the rate limit is now **100 reads / 30 writes a minute per
  account** shared by every key, and market updates moved to per-market realtime topics with one-second batches and full `asOf`-versioned
  books. There are no fees of any kind, sells are canonicalized to complement buys, netting pays 1 per cancelled pair at once,
  multi-outcome markets may be independent, the valuation price behind the live leaderboard is undocumented, and the Sponsor keeps
  wide discretion over settlement, liquidity, and bots. All of it is in `docs/API.md` and ERRATA E21–E27.
- **Strategy decision: start the scenario book gradually.** The current targets need about 720,000 shares across six cheap markets,
  while only about 3,300 shares were offered at acceptable prices on day one, and other players already rest tens of thousands of
  shares of bids in the same markets. Buying into that would move the price against us for nothing. So: begin the book on **Oct 7
  at 30% of equity**, and reach the full 60% from **Oct 21**. Related fixes: the tourney threshold must use the **3,641 members**,
  not the 166 ranked; the rust-live paper run starts Oct 1–2 with cutover after a 24 h review; the rate limiter is retuned to the
  100/30 per-minute budget.
