"""Draft the market map from the newest recorded Cup listing and the latest reference quotes.

The draft always goes to `state/research/market_map.draft.json`. It is copied to the live
`state/market_map.json` only when no live map exists yet or when explicitly promoted, and even
then every human-verified row of the live map is carried over untouched.
"""

from __future__ import annotations

from collections.abc import Sequence

from susq.collectors.quotes import RefQuote
from susq.contracts import ops_pb2
from susq.mapping.cup import load_latest
from susq.mapping.draft import build_market_map
from susq.mapping.io import read_market_map, write_market_map
from susq.research_env import Paths, ResearchConfig, SeqCounter


def draft_map(
    paths: Paths, cfg: ResearchConfig, quotes: Sequence[RefQuote], *, now_ns: int, promote: bool = False
) -> tuple[ops_pb2.MarketMap, bool]:
    """(map, written_live). Raises FileNotFoundError when no Cup listing has been recorded."""
    markets = load_latest(paths.data / "cup")
    if not markets:
        raise FileNotFoundError(f"no Cup listing under {paths.data / 'cup' / 'markets'}")
    live = read_market_map(paths.market_map)
    mm = build_market_map(
        markets,
        quotes,
        seq=SeqCounter(paths.research_state / "seq-market-map.txt").next(),
        existing=live,
        mapping_error_pts=cfg.fair.mapping_error_pts,
        now_ns=now_ns,
    )
    write_market_map(paths.market_map_draft, mm)
    write_live = promote or live is None
    if write_live:
        write_market_map(paths.market_map, mm)
    return mm, write_live
