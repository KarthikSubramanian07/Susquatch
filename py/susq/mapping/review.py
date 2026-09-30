"""Human review of the market map: a checklist report and the command Karthik runs to verify rows.

Nothing here runs on its own. `susq research rules-review` writes `reports/rules_review.md`;
`susq research verify` is the only path that sets `verified: true`, and only when a human runs it.

A race is *clean* when every one of its legs is a two-party race_party row, matched exactly on both
reference venues at full confidence, with no RCV, runoff, independent or manual-settlement flag.
`verify --clean` approves clean races whole, so the constraints racer never sees half a bundle.
"""

from __future__ import annotations

import re
from collections import defaultdict
from collections.abc import Iterable

from susq.contracts import ops_pb2

_EXACT = re.compile(r"\bkalshi=exact\(.*\bpolymarket=exact\(")


def risks(row: ops_pb2.MarketRule) -> list[str]:
    """Why a row needs a human look (empty when it is clean)."""
    out = []
    if row.kind != "race_party":
        out.append(row.kind or "unparsed")
    if row.manual_settlement:
        out.append("manual settlement")
    if row.independent_can_win:
        out.append("independent can win")
    if row.rcv:
        out.append("ranked-choice")
    if row.runoff_possible:
        out.append("runoff")
    if row.kalshi_inverted or row.polymarket_inverted:
        out.append("inverted reference")
    if not row.notes.startswith("confidence=1.00"):
        out.append("low confidence")
    if not _EXACT.search(row.notes):
        out.append("one venue only")
    if row.party not in {"DEM", "REP"}:
        out.append(f"party {row.party or '?'}")
    return out


def races(mm: ops_pb2.MarketMap) -> dict[str, list[ops_pb2.MarketRule]]:
    by: dict[str, list[ops_pb2.MarketRule]] = defaultdict(list)
    for row in mm.markets:
        by[row.race_id or f"unparsed:{row.market_id}"].append(row)
    return dict(by)


def clean_races(mm: ops_pb2.MarketMap) -> list[str]:
    """Races whose legs are all clean and form exactly one DEM + one REP pair."""
    out = []
    for race_id, legs in races(mm).items():
        if sorted(r.party for r in legs) == ["DEM", "REP"] and not any(risks(r) for r in legs):
            out.append(race_id)
    return sorted(out)


def verify(
    mm: ops_pb2.MarketMap, *, market_ids: Iterable[str] = (), race_ids: Iterable[str] = ()
) -> list[str]:
    """Set verified on the named markets and on every leg of the named races. Returns the ids changed."""
    want_m, want_r = {str(m) for m in market_ids}, set(race_ids)
    known = {r.market_id for r in mm.markets} | {r.race_id for r in mm.markets}
    missing = sorted((want_m | want_r) - known)
    if missing:
        raise KeyError(f"not in the map: {', '.join(missing)}")
    changed = []
    for row in mm.markets:
        if (row.market_id in want_m or row.race_id in want_r) and not row.verified:
            row.verified = True
            changed.append(row.market_id)
    return changed


def render(mm: ops_pb2.MarketMap) -> str:
    """The checklist: risky races first, then clean ones, one line per leg."""
    grouped = races(mm)
    clean = set(clean_races(mm))
    order = sorted(grouped, key=lambda r: (r in clean, r))
    done = sum(r.verified for r in mm.markets)
    lines = [
        "# Market rules review",
        "",
        f"Rows: {len(mm.markets)}. Races: {len(grouped)}. Verified rows: {done}. Clean races: {len(clean)}.",
        "",
        "Only verified rows trade. Approve every clean race at once, or name races or market ids:",
        "",
        "```bash",
        "uv run susq research verify --clean",
        "uv run susq research verify 2026-SEN-NE 2026-GOV-GA 191",
        "```",
        "",
        "For risky races, read the market's Info tab on the site first. Paste it into",
        "`state/info_tabs/<market id>.md` when it says something that changes the flags",
        "(who can win, runoffs, ranked-choice, when it settles).",
        "",
        "| ✓ | race | market | party | title | needs a look | reference |",
        "|---|---|---|---|---|---|---|",
    ]
    for race_id in order:
        for row in sorted(grouped[race_id], key=lambda r: r.party):
            venues = (row.kalshi_ticker, "poly" if row.polymarket_token_id else "")
            ref = " ".join(t for t in venues if t) or "none"
            why = ", ".join(risks(row)) or "clean"
            lines.append(
                f"| {'x' if row.verified else ' '} | {race_id} | {row.market_id} | {row.party} "
                f"| {row.title} | {why} | {ref} |"
            )
    return "\n".join(lines) + "\n"
