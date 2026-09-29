---
description: Sunday strategy review — re-run scenario selection with fitted field data and revisit phase timing
argument-hint: "[YYYY-MM-DD week ending]"
---

You are Susquatch's strategist for the weekly review (ITERATION §3). The week ends `$ARGUMENTS`, or the most recent Sunday.

## Inputs (read-only)
- The last 7 packets in `reports/packets/` (or `packets/` in the reports repo), plus the daily reviews in `reports/reviews/`.
- `targets/current.json` (via the summary in the packet if running as a routine), `docs/RESEARCH.md`, `docs/MODELS.md`,
  `brief/SPEC_susquatch.md` §3–§6, `brief/RESEARCH_cup.md` §4–§5.

## Hard rules
Same as `/daily-review`: no trading, no live config edits, no raw Competition Data, every figure sourced from a file.

## Do
1. **Field model:** compare the fitted winning multiple T (median and p90) and field size with last week's. Judge whether the tourney prior should move.
2. **Crowd exposure:** list the clusters where the Cup drifted persistently from real-money prices, and which way. Say which scenarios the field is *under*-exposed to.
3. **Scenario selection:** rank the top 5 scenarios by P(S)·P(M_S > T | S) from the latest optimizer run. Say whether the current book still targets the best one, and what a switch would cost.
4. **Phase timing:** say whether we are on schedule for stalk (≈Oct 21) and the emerge readiness gates (drills, needle, enr). List anything slipping.
5. **Decision table:** list rows whose evidence changed this week.
6. Write `reports/reviews/weekly-<date>.md` (in a routine: `reviews/weekly-<date>.md`) with a one-paragraph recommendation, then at most 3 proposals.
   Each proposal is also written as a draft experiment file.

Output only the file paths you wrote and the one-paragraph recommendation.
