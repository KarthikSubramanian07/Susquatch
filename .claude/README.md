# Claude Code commands for Susquatch

Susquatch's runtime is **LLM-free**. There is no model in the trading path and no API key spend. These slash commands are the
analyst layer. Karthik runs them in an interactive Claude Code session under his subscription, or as scheduled cloud routines
that read the private `susquatch-reports` repo. Each command writes **files only**: reviews, proposals, a draft market map, a draft write-up.

| Command | When | Writes |
|---|---|---|
| `/parse-rules` | at the start, and whenever new markets appear | `state/market_map.json` (draft), `reports/rules_review.md` |
| `/daily-review` | daily (cloud routine) | `reports/reviews/daily-<date>.md`, `experiments/EXP-draft-*` |
| `/weekly-strategy` | Sundays (cloud routine) | `reports/reviews/weekly-<date>.md` |
| `/draft-writeup` | after the lock | `docs/WRITEUP.md` |
| `/go-live` | once, when Karthik says "go live" | `ops/GO_LIVE`, `.env` mode |

None of them can place orders or change live limits. Every change goes through `experiments/` and a PR.
