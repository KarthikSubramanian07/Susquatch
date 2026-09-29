"""Draft `state/market_map.json` (`susq.v1.MarketMap`) from Cup markets plus reference venues.

Matching is an exact join on (race_id, party) produced by the shared parser, so every decision
is explainable. Each row's `notes` records the evidence and a confidence score. Rows are
**never** marked verified: a human does that via `/parse-rules`. When merging with an existing map,
a human-verified row is kept exactly as it is.
"""

from __future__ import annotations

import logging
from collections import defaultdict
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field

from susq.collectors.quotes import KALSHI, POLYMARKET, RefQuote
from susq.contracts import header, ops_pb2
from susq.mapping.cup import CupExchange, CupMarket, node_text, resolution_source
from susq.mapping.geo import RCV_STATES, RUNOFF_STATES, clusters_for
from susq.mapping.races import DEM, IND, REP, RaceKey, parse_party, parse_race
from susq.research_env import PRODUCER

log = logging.getLogger(__name__)

# Races where a non-major-party candidate is known to be competitive (both party markets can lose).
INDEPENDENT_RACES = frozenset({"2026-SEN-NE"})
IND_MATERIAL_PROB = 0.01
_COMPLEMENT = {DEM: REP, REP: DEM}


@dataclass
class _Evidence:
    confidence: float = 1.0
    notes: list[str] = field(default_factory=list)

    def penalize(self, amount: float, note: str) -> None:
        self.confidence = max(0.0, self.confidence - amount)
        self.notes.append(note)


def _index(quotes: Iterable[RefQuote]) -> dict[tuple[str, str, str], RefQuote]:
    """(venue, race_id, party) -> most liquid quote."""
    out: dict[tuple[str, str, str], RefQuote] = {}
    for q in quotes:
        key = (q.venue, q.race_id, q.party)
        if key not in out or q.volume > out[key].volume:
            out[key] = q
    return out


def _independent_races(quotes: Iterable[RefQuote]) -> set[str]:
    live = {q.race_id for q in quotes if q.party == IND and q.mid >= IND_MATERIAL_PROB}
    return live | set(INDEPENDENT_RACES)


def _parse_exchange(market: CupMarket, ex: CupExchange) -> tuple[RaceKey | None, str | None, str]:
    """Race and party for one exchange; returns the evidence source used."""
    text = market.title
    race = parse_race(text)
    source = "title"
    if race is None and market.nodes:
        race, source = parse_race(node_text(market.nodes)), "nodes"
    party = parse_party(ex.option) if (ex.option and len(market.exchanges) > 1) else parse_party(text)
    return race, party, source


def _reference(
    index: dict[tuple[str, str, str], RefQuote], venue: str, race: RaceKey, party: str, allow_complement: bool
) -> tuple[RefQuote | None, bool]:
    if q := index.get((venue, race.race_id, party)):
        return q, False
    comp = _COMPLEMENT.get(party)
    if allow_complement and comp and (q := index.get((venue, race.race_id, comp))):
        return q, True
    return None, False


def _rule_for(
    market: CupMarket,
    ex: CupExchange,
    index: dict[tuple[str, str, str], RefQuote],
    ind_races: set[str],
    mapping_error_pts: float,
) -> ops_pb2.MarketRule:
    ev = _Evidence()
    rule = ops_pb2.MarketRule(
        market_id=market.market_id, exchange_id=ex.exchange_id, title=market.title, verified=False
    )
    rule.resolution_source = resolution_source(market.nodes)
    race, party, source = _parse_exchange(market, ex)
    if race is None or party is None:
        rule.kind = "freeform"
        ev.penalize(1.0, "unparsed: no single race+party in title/nodes")
        rule.notes = f"confidence={ev.confidence:.2f}; " + "; ".join(ev.notes)
        return rule
    if source != "title":
        ev.penalize(0.1, f"race parsed from {source}")
    rule.race_id, rule.kind, rule.party = race.race_id, race.kind, party
    if race.kind == "chamber":
        rule.manual_settlement = True
        rule.clusters.extend(["national", "house"] if race.race_id.endswith("HOUSE") else ["national"])
    else:
        rule.clusters.extend(clusters_for(race.state, house=race.office == "HOUSE"))
        rule.independent_can_win = race.race_id in ind_races
        rule.rcv = race.state == "AK" or (race.state in RCV_STATES and race.office in {"SEN", "HOUSE"})
        rule.runoff_possible = race.state in RUNOFF_STATES
    if party == IND and not rule.independent_can_win:
        rule.independent_can_win = True
    allow_complement = not rule.independent_can_win and party in _COMPLEMENT

    mids: list[float] = []
    for venue in (KALSHI, POLYMARKET):
        q, inverted = _reference(index, venue, race, party, allow_complement)
        if q is None:
            ev.notes.append(f"{venue}=none")
            continue
        if venue == KALSHI:
            rule.kalshi_ticker, rule.kalshi_inverted = q.venue_market_id, inverted
        else:
            rule.polymarket_token_id, rule.polymarket_inverted = q.venue_market_id, inverted
        mids.append(1.0 - q.mid if inverted else q.mid)
        ev.notes.append(f"{venue}={'complement' if inverted else 'exact'}({race.race_id},{party})")
        if inverted:
            ev.penalize(0.1, f"{venue} inverted: only the {_COMPLEMENT[party]} contract is listed")
    if not mids:
        ev.penalize(0.4, "no real-money reference: fair value will be sim-only")
    elif ex.latest_price is not None:
        ref = sum(mids) / len(mids)
        gap = abs(ex.latest_price - ref) * 100
        if gap > mapping_error_pts:
            ev.penalize(0.5, f"PRICE_DISAGREES cup={ex.latest_price:.3f} ref={ref:.3f} ({gap:.0f} pts)")
    if rule.independent_can_win:
        ev.notes.append("independent can win: D+R party markets are not complementary")
    rule.notes = f"confidence={ev.confidence:.2f}; " + "; ".join(ev.notes)
    return rule


def build_market_map(
    markets: Sequence[CupMarket],
    quotes: Sequence[RefQuote],
    *,
    seq: int,
    existing: ops_pb2.MarketMap | None = None,
    mapping_error_pts: float = 25.0,
    now_ns: int | None = None,
) -> ops_pb2.MarketMap:
    """Build the draft map. Human-verified rows in `existing` are preserved untouched."""
    index = _index(quotes)
    ind_races = _independent_races(quotes)
    verified = {(r.market_id, r.exchange_id): r for r in (existing.markets if existing else []) if r.verified}
    out = ops_pb2.MarketMap(header=header(PRODUCER, seq, now_ns=now_ns))
    by_race: dict[str, list[str]] = defaultdict(list)
    for market in sorted(markets, key=lambda m: (len(m.market_id), m.market_id)):
        for ex in market.exchanges:
            key = (market.market_id, ex.exchange_id)
            if key in verified:
                out.markets.append(verified[key])
                continue
            rule = _rule_for(market, ex, index, ind_races, mapping_error_pts)
            out.markets.append(rule)
            if rule.race_id:
                by_race[f"{rule.race_id}/{rule.party}"].append(rule.market_id)
    for race_party, ids in by_race.items():
        if len(ids) > 1:
            log.warning("several Cup markets map to %s: %s (check /parse-rules)", race_party, ids)
    return out


def summarize(mm: ops_pb2.MarketMap) -> dict[str, int]:
    """Counts for logs and the CLI."""
    rows = list(mm.markets)
    return {
        "rows": len(rows),
        "verified": sum(r.verified for r in rows),
        "mapped": sum(bool(r.race_id) for r in rows),
        "with_kalshi": sum(bool(r.kalshi_ticker) for r in rows),
        "with_polymarket": sum(bool(r.polymarket_token_id) for r in rows),
        "freeform": sum(r.kind == "freeform" for r in rows),
        "flagged": sum("PRICE_DISAGREES" in r.notes for r in rows),
    }
