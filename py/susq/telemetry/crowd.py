"""Crowd-position inference: where is the field loaded? (ITERATION §2.3)

For every mapped Cup market, compare the Cup mid with the real-money reference for the same
(race, party), hour by hour. Drift is expressed in the **Democratic direction** (points):

    drift = (cup_mid − ref_mid) × 100 × s,   s = +1 for DEM/IND (anti-R) legs, −1 for REP legs

so a positive drift anywhere means "the Cup is more Democratic than real money". A market's drift
is *persistent* when it has ≥12 hourly bins, |t| ≥ 3 on the hourly means, and ≥75% of bins share
its sign (hourly bins are autocorrelated; the thresholds are deliberately strict). Cluster drift
is the mean over markets carrying that cluster tag.

`exposure(signs, drift)` turns cluster drifts into a field-exposure score in [−1, 1] for a
scenario: positive = the field is already loaded on that scenario (it will crowd the top of the
leaderboard if it happens), negative = the field is leaning against it. No data → 0 (neutral).
"""

from __future__ import annotations

import math
from collections import defaultdict
from collections.abc import Mapping
from dataclasses import asdict, dataclass, field
from typing import Any

import numpy as np
import polars as pl

from susq.contracts import ops_pb2
from susq.mapping.races import REP
from susq.telemetry.cupdata import with_cup_mid

MIN_BINS = 12
MIN_ABS_T = 3.0
MIN_SAME_SIGN = 0.75
EXPOSURE_SCALE_PTS = 3.0
NAMED_CLUSTER_WEIGHT = 0.5
T_CAP = 99.0


@dataclass(frozen=True)
class MarketDrift:
    exchange_id: str
    race_id: str
    party: str
    bins: int
    mean_pts: float
    t_stat: float
    same_sign_frac: float
    persistent: bool


@dataclass
class CrowdSummary:
    markets: list[MarketDrift] = field(default_factory=list)
    cluster_drift_pts: dict[str, float] = field(default_factory=dict)
    persistent_clusters: dict[str, float] = field(default_factory=dict)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


def reference_hourly(ref: pl.DataFrame) -> pl.DataFrame:
    """Hourly mean reference mid per race and party: `hour, race_id, party, ref_mid`."""
    return (
        ref.with_columns(pl.col("ts").dt.truncate("1h").alias("hour"))
        .group_by("hour", "race_id", "party")
        .agg(pl.col("mid").mean().alias("ref_mid"))
    )


def infer_drift(prices: pl.DataFrame, reference: pl.DataFrame, market_map: ops_pb2.MarketMap) -> CrowdSummary:
    """Per-market and per-cluster drift of Cup mids versus real money (raw reference snapshots)."""
    if reference.is_empty():
        return CrowdSummary()
    return infer_drift_hourly(prices, reference_hourly(reference), market_map)


def infer_drift_hourly(
    prices: pl.DataFrame, hourly: pl.DataFrame, market_map: ops_pb2.MarketMap
) -> CrowdSummary:
    """Like `infer_drift`, from a reference already aggregated by `reference_hourly` (see telemetry.job)."""
    rules = {r.exchange_id: r for r in market_map.markets if r.race_id and r.party}
    if prices.is_empty() or hourly.is_empty() or not rules:
        return CrowdSummary()
    link = pl.DataFrame(
        {
            "exchange_id": list(rules),
            "race_id": [r.race_id for r in rules.values()],
            "party": [r.party for r in rules.values()],
            "sign": [-1.0 if r.party == REP else 1.0 for r in rules.values()],
        }
    )
    cup = (
        with_cup_mid(prices)
        .drop_nulls("cup_mid")
        .with_columns(pl.col("ts").dt.truncate("1h").alias("hour"))
        .group_by("hour", "exchange_id")
        .agg(pl.col("cup_mid").mean())
        .join(link, on="exchange_id")
        .join(hourly, on=["hour", "race_id", "party"])
        .with_columns(((pl.col("cup_mid") - pl.col("ref_mid")) * 100 * pl.col("sign")).alias("drift"))
    )
    summary = CrowdSummary()
    by_cluster: dict[str, list[float]] = defaultdict(list)
    persistent_by_cluster: dict[str, list[float]] = defaultdict(list)
    for (exchange_id,), grp in cup.group_by(["exchange_id"]):
        d = grp["drift"].to_numpy()
        n = d.size
        mean = float(d.mean())
        sd = float(d.std(ddof=1)) if n > 1 else 0.0
        # A constant nonzero drift has infinite t; cap it so the JSON stays finite.
        t = mean / (sd / math.sqrt(n)) if sd > 0 else math.copysign(T_CAP, mean) * bool(mean)
        t = max(-T_CAP, min(T_CAP, t))
        same = float(np.mean(np.sign(d) == np.sign(mean))) if mean else 0.0
        persistent = n >= MIN_BINS and abs(t) >= MIN_ABS_T and same >= MIN_SAME_SIGN
        rule = rules[str(exchange_id)]
        summary.markets.append(
            MarketDrift(str(exchange_id), rule.race_id, rule.party, n, mean, float(t), same, persistent)
        )
        for cluster in rule.clusters or ["national"]:
            by_cluster[cluster].append(mean)
            if persistent:
                persistent_by_cluster[cluster].append(mean)
    summary.markets.sort(key=lambda m: -abs(m.mean_pts))
    summary.cluster_drift_pts = {c: float(np.mean(v)) for c, v in sorted(by_cluster.items())}
    summary.persistent_clusters = {c: float(np.mean(v)) for c, v in sorted(persistent_by_cluster.items())}
    return summary


def exposure(signs: Mapping[str, int], cluster_drift_pts: Mapping[str, float]) -> float:
    """Field exposure to a scenario whose shock signs are `signs` (+1 = D-favorable), in [−1, 1]."""
    total = 0.0
    for cluster, sign in signs.items():
        if cluster in cluster_drift_pts:
            weight = 1.0 if cluster == "national" else NAMED_CLUSTER_WEIGHT
            total += weight * sign * cluster_drift_pts[cluster]
    return math.tanh(total / EXPOSURE_SCALE_PTS)
