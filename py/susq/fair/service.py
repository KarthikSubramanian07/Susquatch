"""Live fair values: `fair = kalshi_weight × real-money mid + sim_weight × sim` → `state/fair_live.json`.

Per market-map row:

* real-money mid: the row's own Kalshi ticker / Polymarket token (so a human's edits in the
  verified map win over our parser), inverted when the row says so, spread-weighted across venues,
  quotes older than `quote_max_age_s` dropped (consumers separately cancel orders built on a
  fair file older than `stale_after_s`);
* sim: the calibrated probability for (race, party) from `state/research/sim_probs.json`;
* whichever exists alone is used alone, and `source` says which;
* the 80% band is built in logit space from the venue half-spread / disagreement and the
  sim-vs-market disagreement.

A row whose Cup mid is more than `mapping_error_pts` from fair is flagged (probable mis-mapping)
in `state/research/fair_flags.json`. The live file itself holds ONLY `header` + `fair`, because
the bridge and rust-live judge staleness from `header.produced_at_ns` and nothing else.
"""

from __future__ import annotations

import json
import logging
import math
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import asdict, dataclass
from pathlib import Path

from susq.calibrate.consensus import MIN_SPREAD
from susq.collectors.quotes import RefQuote
from susq.contracts import header, ops_pb2, targets_pb2, to_json
from susq.research_env import PRODUCER, FairConfig, SeqCounter, atomic_write_text

log = logging.getLogger(__name__)

Z80 = 1.2815515655446004
SIM_ONLY_SIGMA = 0.08
MIN_SIM_SIGMA = 0.02
P_EPS = 1e-4


@dataclass(frozen=True)
class FairFlag:
    exchange_id: str
    market_id: str
    title: str
    cup_mid: float
    fair: float
    gap_pts: float
    reason: str


def _logit(p: float) -> float:
    p = min(max(p, P_EPS), 1 - P_EPS)
    return math.log(p / (1 - p))


def _expit(x: float) -> float:
    return 1.0 / (1.0 + math.exp(-x))


def _market_mid(rule: ops_pb2.MarketRule, quotes: Mapping[str, RefQuote]) -> tuple[float, float, str] | None:
    """(p, sigma, venues) from the row's own reference ids."""
    legs: list[tuple[float, float, str]] = []
    for ref_id, inverted, venue in (
        (rule.kalshi_ticker, rule.kalshi_inverted, "kalshi"),
        (rule.polymarket_token_id, rule.polymarket_inverted, "polymarket"),
    ):
        if ref_id and (q := quotes.get(ref_id)) is not None:
            legs.append(((1.0 - q.mid) if inverted else q.mid, max(q.spread, MIN_SPREAD), venue))
    if not legs:
        return None
    weights = [1.0 / s for _, s, _ in legs]
    total = sum(weights)
    p = sum(w * m for w, (m, _, _) in zip(weights, legs, strict=True)) / total
    half_spread = sum(w * s / 2 for w, (_, s, _) in zip(weights, legs, strict=True)) / total
    disagreement = (max(m for m, _, _ in legs) - min(m for m, _, _ in legs)) / 2
    return p, max(half_spread, disagreement, MIN_SPREAD / 2), "+".join(v for _, _, v in legs)


def fair_for_rule(
    rule: ops_pb2.MarketRule,
    quotes: Mapping[str, RefQuote],
    sim_probs: Mapping[str, Mapping[str, float]],
    cfg: FairConfig,
) -> targets_pb2.FairValue | None:
    """Blend real money and sim for one row, or None when neither exists."""
    if not rule.race_id or not rule.party:
        return None
    market = _market_mid(rule, quotes)
    sim = sim_probs.get(rule.race_id, {}).get(rule.party)
    if market is None and sim is None:
        return None
    if market is not None and sim is not None:
        mp, msig, venues = market
        wk, ws = cfg.kalshi_weight, cfg.sim_weight
        wk, ws = wk / (wk + ws), ws / (wk + ws)
        p = wk * mp + ws * sim
        ssig = max(abs(sim - mp), MIN_SIM_SIGMA)
        sig_logit = math.hypot(wk * msig, ws * ssig) / max(p * (1 - p), P_EPS)
        source = f"{cfg.kalshi_weight:g}*market({venues})+{cfg.sim_weight:g}*sim"
    elif market is not None:
        p, msig, venues = market
        sig_logit = msig / max(p * (1 - p), P_EPS)
        source = f"market({venues})"
    else:
        assert sim is not None
        p, sig_logit, source = sim, SIM_ONLY_SIGMA / max(sim * (1 - sim), P_EPS), "sim"
    lo, hi = _expit(_logit(p) - Z80 * sig_logit), _expit(_logit(p) + Z80 * sig_logit)
    return targets_pb2.FairValue(
        exchange_id=rule.exchange_id,
        market_id=rule.market_id,
        p_yes=p,
        p_lo=min(lo, p),
        p_hi=max(hi, p),
        source=source,
    )


def compute_fair(
    rules: Iterable[ops_pb2.MarketRule],
    quotes: Iterable[RefQuote],
    sim_probs: Mapping[str, Mapping[str, float]],
    cfg: FairConfig,
    *,
    now_ns: int,
    cup_mids: Mapping[str, float] | None = None,
) -> tuple[list[targets_pb2.FairValue], list[FairFlag]]:
    """Fair values for every mappable row, plus mapping-error flags against Cup mids."""
    fresh: dict[str, RefQuote] = {}
    for q in quotes:
        if now_ns - q.ts_ns > cfg.quote_max_age_s * 1e9:
            continue  # stale: never price off an old real-money quote
        if q.venue_market_id not in fresh or q.ts_ns > fresh[q.venue_market_id].ts_ns:
            fresh[q.venue_market_id] = q
    fairs: list[targets_pb2.FairValue] = []
    flags: list[FairFlag] = []
    by_exchange = {r.exchange_id: r for r in rules}
    for rule in by_exchange.values():
        fv = fair_for_rule(rule, fresh, sim_probs, cfg)
        if fv is None:
            continue
        fairs.append(fv)
        cup = (cup_mids or {}).get(rule.exchange_id)
        if cup is not None and abs(cup - fv.p_yes) * 100 > cfg.mapping_error_pts:
            flags.append(
                FairFlag(
                    rule.exchange_id, rule.market_id, rule.title, cup, fv.p_yes,
                    (cup - fv.p_yes) * 100, "mapping_error: |cup_mid - fair| > mapping_error_pts",
                )
            )  # fmt: skip
    return fairs, flags


def fair_file(fairs: Sequence[targets_pb2.FairValue], *, seq: int, now_ns: int) -> targets_pb2.TargetFile:
    """The live file: header + fair, nothing else."""
    tf = targets_pb2.TargetFile(header=header(PRODUCER, seq, now_ns=now_ns))
    tf.fair.extend(fairs)
    return tf


class FairService:
    """Computes and atomically writes `state/fair_live.json` (plus flags) on each `run_once`."""

    def __init__(
        self,
        *,
        out_path: Path,
        flags_path: Path,
        seq: SeqCounter,
        cfg: FairConfig,
        load_rules: Callable[[], Sequence[ops_pb2.MarketRule]],
        load_quotes: Callable[[], Sequence[RefQuote]],
        load_sim: Callable[[], Mapping[str, Mapping[str, float]]],
        load_cup_mids: Callable[[], Mapping[str, float]] = dict,
        clock_ns: Callable[[], int] = time.time_ns,
    ) -> None:
        self.out_path, self.flags_path, self.seq, self.cfg = out_path, flags_path, seq, cfg
        self._rules, self._quotes, self._sim, self._cup = load_rules, load_quotes, load_sim, load_cup_mids
        self._clock = clock_ns

    def run_once(self) -> tuple[int, int]:
        """Write one fair file; returns (n_fair, n_flags)."""
        now = self._clock()
        fairs, flags = compute_fair(
            self._rules(), self._quotes(), self._sim(), self.cfg, now_ns=now, cup_mids=self._cup()
        )
        tf = fair_file(fairs, seq=self.seq.next(), now_ns=now)
        atomic_write_text(self.out_path, to_json(tf))
        atomic_write_text(
            self.flags_path,
            json.dumps({"produced_at_ns": now, "flags": [asdict(f) for f in flags]}, indent=1),
        )
        for f in flags:
            log.warning("fair flag %s %s: cup %.3f vs fair %.3f", f.exchange_id, f.title, f.cup_mid, f.fair)
        return len(fairs), len(flags)
