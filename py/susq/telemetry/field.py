"""The field's cheap-leg footprint from the trade tape, and the resolved field maximum it implies (EXP-004).

The leaderboard shows marks; the Cup ranks on the balance after every market resolves. The tape
(`data/cup/tape`) shows what the field actually bought, at what price, per contract, but carries no
account ids. This module turns it into a model of the resolved winning multiple T(ω):

1. **Price ladder.** Per exchange, taker side and price: shares and trade count. Built one `date=`
   partition at a time (`read_tape_ladder`), so peak memory is one day of trades, never the tape.
2. **Price space.** The platform runs a single YES-space book (docs/API.md; brief/ERRATA E4; the mock's
   `/trades`), and sells are canonicalised into the complementary buy, so `side` is the side the taker
   *bought* and `price` is the YES price. `detect_price_space` checks this on the data: in YES space the
   VWAPs of YES-taker and NO-taker trades on one exchange sit close together; in taker space they sum to
   about one. The footprint records the votes and only switches to taker space on a strict majority.
3. **Cheap-leg footprint.** For contract side c = (exchange, side) bought at side price q
   (q = price for YES, 1 − price for NO in YES space), spend = shares · q. Only q ≤ `CHEAP_MAX` (0.20)
   counts: that is where resolved multiples of 5x and more come from. Per side,
   S_c = Σ spend, p̄_c = S_c / Σ shares (the price-weighted mean entry, so an all-in account at p̄_c
   resolves at 1/p̄_c when c pays).
4. **All-in counts.** stake = max(100,000, median leaderboard volume of accounts that turned over at
   least 100% of their balance); n_c = min(S_c / stake, n_hi) with n_hi that account count, and the
   n_c rescaled so that Σ n_c ≤ n_hi.
5. **Resolved maximum.** In world ω, T(ω) = max(consensus/bot maximum, floor, max over contract sides
   that pay in ω and are held of 1/p̄_c), where side c is held with probability min(1, n_c) (drawn per
   world). A side pays when its YES leg resolves YES (YES side) or NO (NO side) in the simulated
   world; a side whose exchange is not simulated pays independently with probability p̄_c.

Only the taker's side is counted. The maker's side of a cheap trade (selling YES at 0.95 is buying NO at
0.05) is mostly resting liquidity from hedged quoters, including our own far-quote ladder, not an
all-in decision. A whole balance bought at 0.95 is not a cheap leg and adds nothing: it resolves to
about 0, or at best 1.05x, which is the point of fitting to the tape instead of the marks.

Everything that leaves this module is aggregate per contract side: no trade ids, no accounts.
"""

from __future__ import annotations

import math
from collections.abc import Mapping, Sequence
from dataclasses import asdict, dataclass, field
from datetime import date
from pathlib import Path
from typing import Any

import numpy as np
import polars as pl

from susq.collectors.store import partition_days, read_snapshots
from susq.models.tourney import FieldParams, consensus_and_bot_max
from susq.models.types import BoolArray, FloatArray

INITIAL_BALANCE = 100_000.0
CHEAP_MAX = 0.20  # side prices at or below this resolve to 5x or more when they pay
HIGH_TURNOVER = 1.0  # volume / balance: "turned over at least 100% of the balance"
BOT_MIN_TRADES = 200  # leaderboard trades_count at or above which an account is counted as a bot
# The footprint drives the threshold only with at least this much tape (below it: the EXP-001 fit).
MIN_TRADES = 1_000
MIN_DAYS = 2.0
MIN_CHEAP_SIDES = 5
_SPACE_MIN_EDGE = 0.15  # exchanges whose YES VWAP sits within 0.15 of 0.5 cannot tell the spaces apart
_SPACE_MIN_SHARES = 10
LADDER_COLUMNS = ("ts", "exchange_id", "trade_id", "side", "price", "size")


# ── the ladder: shares per (exchange, taker side, price), folded per day ────────────────────


def price_ladder(trades: pl.DataFrame) -> pl.DataFrame:
    """(exchange_id, side, price) -> shares, trades, first_ns, last_ns for one batch of tape rows.

    Rows without a side, a price inside (0, 1) or a positive size are dropped; repeats of a trade_id
    (a recording pass that was cut short and redone) count once.
    """
    if trades.is_empty():
        return _empty_ladder()
    df = (
        trades.unique(subset="trade_id", keep="first")
        .with_columns(
            pl.col("exchange_id").cast(pl.Utf8),
            pl.col("side").cast(pl.Utf8).str.to_uppercase(),
            pl.col("price").cast(pl.Float64).round(4),
            pl.col("size").cast(pl.Float64),
            pl.col("ts").dt.epoch("ns").alias("ts_ns"),
        )
        .filter(
            pl.col("side").is_in(["YES", "NO"])
            & (pl.col("price") > 0.0)
            & (pl.col("price") < 1.0)
            & (pl.col("size") > 0)
        )
    )
    return df.group_by("exchange_id", "side", "price").agg(
        pl.col("size").sum().alias("shares"),
        pl.len().cast(pl.Int64).alias("trades"),
        pl.col("ts_ns").min().alias("first_ns"),
        pl.col("ts_ns").max().alias("last_ns"),
    )


def _empty_ladder() -> pl.DataFrame:
    return pl.DataFrame(
        schema={
            "exchange_id": pl.Utf8, "side": pl.Utf8, "price": pl.Float64, "shares": pl.Float64,
            "trades": pl.Int64, "first_ns": pl.Int64, "last_ns": pl.Int64,
        }
    )  # fmt: skip


def merge_ladders(ladders: Sequence[pl.DataFrame]) -> pl.DataFrame:
    frames = [ladder for ladder in ladders if not ladder.is_empty()]
    if not frames:
        return _empty_ladder()
    return (
        pl.concat(frames, how="vertical_relaxed")
        .group_by("exchange_id", "side", "price")
        .agg(
            pl.col("shares").sum(),
            pl.col("trades").sum(),
            pl.col("first_ns").min(),
            pl.col("last_ns").max(),
        )
        .sort("exchange_id", "side", "price")
    )


def read_tape_ladder(tape_root: Path, *, since: date | None = None) -> pl.DataFrame:
    """The whole tape's price ladder, folded one `date=` partition at a time.

    A trade's partition is its own `createdAt` day, so a trade_id never spans two partitions and the
    per-day dedupe is exact. Peak memory is one day of trades plus the (small) ladder.
    """
    ladders = []
    for day, _ in partition_days(tape_root, since=since):
        frame = read_snapshots(tape_root, since=day, until=day, columns=LADDER_COLUMNS)
        if not frame.is_empty():
            ladders.append(price_ladder(frame))
    return merge_ladders(ladders)


# ── price space ─────────────────────────────────────────────────────────────────────────────


def detect_price_space(ladder: pl.DataFrame) -> tuple[str, dict[str, int]]:
    """("yes" | "taker", votes): which space the tape's `price` is quoted in.

    Per exchange with both YES- and NO-taker trades and a YES-taker VWAP a away from 0.5, with NO-taker
    VWAP b: |a − b| < |a + b − 1| votes YES space, the reverse votes taker space. YES space unless taker
    space wins a strict majority (the documented convention, and what the mock serves).
    """
    votes = {"yes": 0, "taker": 0}
    if ladder.is_empty():
        return "yes", votes
    vwap = (
        ladder.group_by("exchange_id", "side")
        .agg(
            ((pl.col("price") * pl.col("shares")).sum() / pl.col("shares").sum()).alias("vwap"),
            pl.col("shares").sum().alias("shares"),
        )
        .filter(pl.col("shares") >= _SPACE_MIN_SHARES)
    )
    yes = vwap.filter(pl.col("side") == "YES").select("exchange_id", pl.col("vwap").alias("a"))
    no = vwap.filter(pl.col("side") == "NO").select("exchange_id", pl.col("vwap").alias("b"))
    for a, b in yes.join(no, on="exchange_id").select("a", "b").iter_rows():
        if abs(a - 0.5) < _SPACE_MIN_EDGE:
            continue
        votes["yes" if abs(a - b) < abs(a + b - 1.0) else "taker"] += 1
    return ("taker" if votes["taker"] > votes["yes"] else "yes"), votes


# ── footprint ───────────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class SideFootprint:
    """The cheap-leg footprint of one contract side (aggregates only)."""

    exchange_id: str
    side: str  # the side bought: YES or NO
    cheap_spend: float  # SUSQies spent at side prices <= CHEAP_MAX
    cheap_shares: float
    p_bar: float  # cheap_spend / cheap_shares: the price-weighted mean entry, in the side's own space
    n_all_in: float  # estimated all-in accounts on this side
    trades: int

    @property
    def multiple(self) -> float:
        """What an all-in account at p̄ resolves to when this side pays."""
        return 1.0 / self.p_bar


@dataclass
class FieldFootprint:
    """Tape aggregates that seed the threshold (telemetry.json `field`)."""

    as_of_ns: int
    trades: int
    days: float  # first to last trade on the tape
    total_spend: float
    cheap_spend: float
    price_space: str
    price_space_votes: dict[str, int]
    stake: float
    n_high_turnover: int
    n_bot: int | None  # accounts at or above BOT_MIN_TRADES; None without a leaderboard
    cheap_max: float = CHEAP_MAX
    sides: list[SideFootprint] = field(default_factory=list)
    usable: bool = False
    reason: str = ""

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, raw: Mapping[str, Any]) -> FieldFootprint:
        body = dict(raw)
        body["sides"] = [SideFootprint(**s) for s in body.get("sides") or []]
        return cls(**body)

    def summary(self) -> str:
        top = max(self.sides, key=lambda s: s.n_all_in, default=None)
        lead = f", heaviest {top.side} at p̄ {top.p_bar:.3f} (n {top.n_all_in:.1f})" if top else ""
        return (
            f"tape {self.trades} trades over {self.days:.1f} d, {len(self.sides)} cheap sides, "
            f"cheap spend {self.cheap_spend:,.0f}, stake {self.stake:,.0f}, n_hi {self.n_high_turnover}, "
            f"space {self.price_space}{lead}"
        )


def leaderboard_counts(leaderboard: pl.DataFrame) -> tuple[float, int, int | None]:
    """(stake, accounts with turnover >= 100%, bot-like accounts) from the newest leaderboard snapshot.

    stake = max(100,000, median volume of the high-turnover accounts): an all-in account spends at
    least its balance, and churn makes the cheap spend on a contract larger than the balance in it.
    """
    if leaderboard.is_empty() or "volume" not in leaderboard.columns:
        return INITIAL_BALANCE, 0, None
    latest = leaderboard.filter(pl.col("ts") == pl.col("ts").max())
    vol = latest["volume"].fill_null(0.0).cast(pl.Float64).to_numpy()
    hi = vol[vol >= HIGH_TURNOVER * INITIAL_BALANCE]
    stake = max(INITIAL_BALANCE, float(np.median(hi))) if hi.size else INITIAL_BALANCE
    n_bot = None
    if "trades_count" in latest.columns:
        n_bot = int((latest["trades_count"].fill_null(0).to_numpy() >= BOT_MIN_TRADES).sum())
    return stake, int(hi.size), n_bot


def build_footprint(
    ladder: pl.DataFrame,
    leaderboard: pl.DataFrame | None = None,
    *,
    as_of_ns: int = 0,
    cheap_max: float = CHEAP_MAX,
) -> FieldFootprint:
    """Cheap-leg spend, p̄ and all-in counts per contract side, plus the usability verdict."""
    space, votes = detect_price_space(ladder)
    stake, n_hi, n_bot = leaderboard_counts(leaderboard if leaderboard is not None else pl.DataFrame())
    if ladder.is_empty():
        return FieldFootprint(
            as_of_ns, 0, 0.0, 0.0, 0.0, space, votes, stake, n_hi, n_bot, cheap_max, [], False, "no tape"
        )
    q = pl.col("price") if space == "taker" else (
        pl.when(pl.col("side") == "YES").then(pl.col("price")).otherwise(1.0 - pl.col("price"))
    )  # fmt: skip
    df = ladder.with_columns(q.alias("q")).with_columns((pl.col("shares") * pl.col("q")).alias("spend"))
    trades = int(df["trades"].sum())
    days = float(df.select((pl.col("last_ns").max() - pl.col("first_ns").min()) / 86_400e9).item())
    cheap = (
        df.filter(pl.col("q") <= cheap_max + 1e-9)
        .group_by("exchange_id", "side")
        .agg(pl.col("spend").sum(), pl.col("shares").sum(), pl.col("trades").sum())
        .filter(pl.col("shares") > 0)
        .sort("exchange_id", "side")
    )
    raw_n = [s / stake for s in cheap["spend"].to_list()]
    if n_hi > 0:
        raw_n = [min(n, float(n_hi)) for n in raw_n]
        total = sum(raw_n)
        if total > n_hi:
            raw_n = [n * n_hi / total for n in raw_n]
    sides = [
        SideFootprint(ex, side, float(spend), float(shares), float(spend / shares), float(n), int(t))
        for (ex, side, spend, shares, t), n in zip(
            cheap.select("exchange_id", "side", "spend", "shares", "trades").iter_rows(), raw_n, strict=True
        )
    ]
    usable, reason = _usable(trades, days, sides)
    return FieldFootprint(
        as_of_ns, trades, days, float(df["spend"].sum()), float(cheap["spend"].sum()), space, votes,
        stake, n_hi, n_bot, cheap_max, sides, usable, reason,
    )  # fmt: skip


def _usable(trades: int, days: float, sides: Sequence[SideFootprint]) -> tuple[bool, str]:
    if trades < MIN_TRADES:
        return False, f"{trades} trades < {MIN_TRADES}"
    if days < MIN_DAYS:
        return False, f"{days:.1f} days of tape < {MIN_DAYS:.0f}"
    if len(sides) < MIN_CHEAP_SIDES:
        return False, f"{len(sides)} cheap sides < {MIN_CHEAP_SIDES}"
    return True, "ok"


# ── the threshold ───────────────────────────────────────────────────────────────────────────


def tape_field_params(field_size: int, footprint: FieldFootprint) -> FieldParams:
    """FieldParams for the non-YOLO types: YOLO is replaced by the tape, bots counted on the leaderboard."""
    n = max(1, field_size)
    base = FieldParams(n_players=n, frac_yolo=0.0)
    if footprint.n_bot is None:
        return base
    return FieldParams(n_players=n, frac_yolo=0.0, frac_bot=min(1.0, footprint.n_bot / n))


def tape_threshold(
    outcomes: BoolArray,
    contracts: Sequence[str],
    footprint: FieldFootprint,
    params: FieldParams,
    rng: np.random.Generator,
    *,
    floor: float = 1.0,
) -> FloatArray:
    """One resolved winning multiple T(ω) per world (rows of `outcomes`, columns = `contracts`).

    T(ω) = max(1, floor, consensus/bot maximum from `params` (its YOLO share is ignored: the tape
    replaces it), max over held contract sides that pay in ω of 1/p̄). Sides are visited from the
    largest multiple down and each world takes the first hit, so memory stays O(worlds).
    """
    worlds = outcomes.shape[0]
    t = np.maximum(consensus_and_bot_max(worlds, replace_yolo(params), rng), max(1.0, floor))
    col = {ex: j for j, ex in enumerate(contracts)}
    hit = np.zeros(worlds)
    open_ = np.ones(worlds, dtype=bool)
    for side in sorted(footprint.sides, key=lambda s: -s.multiple):
        if side.n_all_in <= 0.0 or side.p_bar <= 0.0:
            continue
        j = col.get(side.exchange_id)
        pays: BoolArray
        if j is None:
            pays = np.less(rng.random(worlds), side.p_bar)
        else:
            pays = outcomes[:, j] if side.side == "YES" else ~outcomes[:, j]
        held = pays & open_
        if side.n_all_in < 1.0:
            held &= rng.random(worlds) < side.n_all_in
        hit[held] = side.multiple
        open_ &= ~held
        if not open_.any():
            break
    return np.asarray(np.maximum(t, hit), dtype=np.float64)


def replace_yolo(params: FieldParams) -> FieldParams:
    if params.frac_yolo == 0.0:
        return params
    return FieldParams(**{**asdict(params), "frac_yolo": 0.0})


def leaderboard_floor(leaderboard_summary: Mapping[str, Any] | None) -> float:
    """The leaderboard anchor kept as a floor: the third-highest mark, at most the EXP-001 fit's T50.

    The third-highest mark is the statistic EXP-001 found least distorted by the 0.95-on-a-1%-outcome
    marks; capping it at the fit's p50 keeps a mark spike from becoming the threshold.
    """
    if not leaderboard_summary:
        return 1.0
    top = leaderboard_summary.get("top_k_multiples") or []
    third = float(top[2]) if len(top) >= 3 else (float(top[-1]) if top else 1.0)
    p50 = (leaderboard_summary.get("threshold") or {}).get("p50")
    return max(1.0, min(third, float(p50)) if p50 is not None and math.isfinite(float(p50)) else third)
