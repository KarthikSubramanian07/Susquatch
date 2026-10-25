"""Endgame rival model (STRATEGY §4, EXP-011): the leaders' likely payout vectors, and how ours relates.

The Cup ranks on the balance after every market resolves; the leaderboard shows a mark. The public tape
carries no account ids, so a rival's book cannot be read off it. What is public per account is the hourly
leaderboard row (P&L, trade count, volume). That is enough to infer the book:

1. **Quiet windows.** Between two consecutive hourly snapshots in which an account's `trades_count` did
   not change, its P&L moved only because its positions were revalued:
   ΔP&L = Σ_m q_m · Δv_m, with q_m the signed shares (+YES, −NO) on exchange m and v_m the YES-space
   valuation price. The price panel v comes from the Cup price recorder (book mid by default; the
   last-trade price is the alternative; on the Oct 15 to 25 tape the mid fits better).
2. **Sparse fit.** Per account, orthogonal matching pursuit over the exchanges whose price moved, with
   recency weights (half-life `half_life_h`), stopped by BIC, one robust refit after trimming windows
   whose residual exceeds `TRIM_MADS` median absolute deviations. Complementary exchanges (the D and R
   legs of one race) are collinear, but they pay identically, so the payout vector is unaffected by
   which one the fit picks.
3. **Payout vector.** In world ω, final balance = balance + Σ_m q_m · (Y_m(ω) − v_m), floored at 0,
   where Y_m is the YES outcome (1/0). This is exact for YES and NO holdings alike and needs no cash
   estimate: whatever the fit cannot see (positions whose price never moved) stays at its mark. That
   is an upper bound: a whole balance bought at 0.95 on a ~1% outcome and marked by the trailing VWAP
   never moves, so it is invisible here. The lower bound (`payout_multiple_low`) lets that unexplained
   value, balance − visible gross, resolve to zero; reports carry both. Y comes
   from the optimizer's own simulator draws (`leg_outcomes`); an exchange the simulator does not cover
   resolves YES with probability v_m, independently, with one shared draw per exchange so rivals who
   hold the same contract stay correlated.
4. **Rival threshold.** R(ω) = max over modeled rivals of their resolved multiple, floored at 1.0. The
   optimizer hook (`[optimizer] rival_decorrelation = w`, default 0.0, off) scores scenarios against
   T_w(ω) = (1 − w) · T(ω) + w · R(ω): w = 1 is P(beat every modeled rival).

Nothing that leaves this module names an account: rivals are slots ordered by current mark ("R01" is the
current leader other than us). Reports are aggregates; the slot books (exchange ids and shares) are
written only to on-box, gitignored paths.
"""

from __future__ import annotations

import hashlib
import math
from collections.abc import Mapping, Sequence
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import numpy as np
import polars as pl

from susq.collectors.store import partition_days, read_snapshots
from susq.contracts import ops_pb2
from susq.models.types import BoolArray, FloatArray, RaceSpec, SimDraws
from susq.optimizer.legs import Leg, leg_outcomes

INITIAL_BALANCE = 100_000.0
NS_PER_HOUR = 3_600 * 10**9
TRIM_MADS = 6.0
MIN_PRICE_MOVE = 1e-6  # an exchange whose price never moved more than this in the windows is invisible
PRICE_COLUMNS = ("ts", "exchange_id", "latest_price", "best_bid", "best_ask")
LONGSHOT_MAX = 0.20  # side price at or below which a holding is a longshot (resolves to 5x+ when it pays)


@dataclass(frozen=True)
class FitConfig:
    lookback_h: float = 240.0  # quiet windows considered: the last ten days
    half_life_h: float = 72.0  # recency weight of a window
    max_positions: int = 25
    min_windows: int = 12
    min_r2: float = 0.30  # below this the book is reported but not modeled
    max_gross_frac: float = 1.5  # Σ |q| · side price above this share of balance is not a plausible book
    min_position_frac: float = 0.002  # positions marked below this share of balance are dropped as noise


# ── price panel ────────────────────────────────────────────────────────────────────────────


@dataclass
class PricePanel:
    """YES-space valuation price per exchange at each leaderboard snapshot (forward-filled)."""

    ts_ns: np.ndarray  # (K,) int64
    exchanges: list[str]
    values: FloatArray  # (K, M), NaN until an exchange first has a price

    def latest(self) -> dict[str, float]:
        out: dict[str, float] = {}
        for j, ex in enumerate(self.exchanges):
            col = self.values[:, j]
            ok = np.flatnonzero(np.isfinite(col))
            if ok.size:
                out[ex] = float(col[ok[-1]])
        return out


def _valuation(frame: pl.DataFrame, source: str) -> pl.DataFrame:
    if frame.is_empty():
        return pl.DataFrame(
            schema={"ts": pl.Datetime("us", "UTC"), "exchange_id": pl.Utf8, "price": pl.Float64}
        )
    frame = frame.with_columns(pl.col("exchange_id").cast(pl.Utf8))
    last = pl.col("latest_price")
    if source == "mid":
        bid, ask = pl.col("best_bid"), pl.col("best_ask")
        two_sided = bid.is_not_null() & ask.is_not_null() & (bid > 0) & (ask < 1) & (bid <= ask)
        price = pl.when(two_sided).then((bid + ask) / 2.0).otherwise(last)
    elif source == "last":
        price = last
    else:
        raise ValueError(f"unknown valuation source {source!r} (mid | last)")
    return (
        frame.select("ts", "exchange_id", price.cast(pl.Float64).alias("price"))
        .filter(pl.col("price").is_not_null() & (pl.col("price") > 0.0) & (pl.col("price") < 1.0))
        .sort("ts")
    )


def price_panel(
    prices: pl.DataFrame,
    snapshot_ts: Sequence[datetime],
    *,
    source: str = "mid",
    lag: timedelta = timedelta(0),
) -> PricePanel:
    """The valuation price of every exchange in `prices` as of each snapshot time (+ `lag`)."""
    return _panel_from_valuations([_valuation(prices, source)], snapshot_ts, lag)


def read_price_panel(
    prices_root: Path, snapshot_ts: Sequence[datetime], *, source: str = "mid", lag: timedelta = timedelta(0)
) -> PricePanel:
    """`price_panel` folded one `date=` partition at a time (per-minute snapshots are large).

    Per day only the last valuation per exchange at or before each snapshot of that day (and the day's
    closing value, which carries into the next day) is kept, so peak memory is one day of snapshots.
    """
    if not snapshot_ts:
        return PricePanel(np.zeros(0, dtype=np.int64), [], np.zeros((0, 0)))
    start = (min(snapshot_ts) + lag - timedelta(days=1)).date()
    kept: list[pl.DataFrame] = []
    for day, _ in partition_days(prices_root, since=start):
        frame = _valuation(read_snapshots(prices_root, since=day, until=day, columns=PRICE_COLUMNS), source)
        if frame.is_empty():
            continue
        kept.append(_compress_day(frame, [t + lag for t in snapshot_ts if (t + lag).date() == day]))
    return _panel_from_valuations(kept, snapshot_ts, lag)


def _compress_day(frame: pl.DataFrame, cut_ts: Sequence[datetime]) -> pl.DataFrame:
    """The last row per exchange before each cut and at the end of the day (enough for an as-of join)."""
    ts_dtype = frame.schema["ts"]
    cuts = pl.Series("cut", sorted(cut_ts), dtype=ts_dtype)
    tagged = frame.join_asof(
        pl.DataFrame({"cut": cuts}).with_columns(pl.col("cut").alias("ts")).sort("ts"),
        on="ts",
        strategy="forward",
    )
    return (
        tagged.group_by("exchange_id", "cut")
        .agg(pl.col("ts").last(), pl.col("price").last())
        .select("ts", "exchange_id", "price")
        .sort("ts")
    )


def _panel_from_valuations(
    frames: Sequence[pl.DataFrame], snapshot_ts: Sequence[datetime], lag: timedelta
) -> PricePanel:
    snaps = sorted(snapshot_ts)
    frames = [f for f in frames if not f.is_empty()]
    if not snaps or not frames:
        return PricePanel(np.array([_ns(t) for t in snaps], dtype=np.int64), [], np.zeros((len(snaps), 0)))
    vals = pl.concat(frames, how="vertical_relaxed").sort("ts")
    exchanges = sorted(vals["exchange_id"].unique().to_list())
    ts_dtype = vals.schema["ts"]
    grid = (
        pl.DataFrame({"ts": pl.Series([t + lag for t in snaps], dtype=ts_dtype)})
        .with_row_index("k")
        .join(pl.DataFrame({"exchange_id": exchanges}), how="cross")
        .sort("ts")
    )
    joined = grid.join_asof(vals, on="ts", by="exchange_id", strategy="backward", check_sortedness=False)
    col = {ex: j for j, ex in enumerate(exchanges)}
    values = np.full((len(snaps), len(exchanges)), np.nan)
    for k, ex, p in joined.select("k", "exchange_id", "price").iter_rows():
        if p is not None:
            values[k, col[ex]] = p
    return PricePanel(np.array([_ns(t) for t in snaps], dtype=np.int64), exchanges, values)


_EPOCH = datetime(1970, 1, 1, tzinfo=UTC)


def _ns(t: datetime) -> int:
    """Exact epoch nanoseconds (a float `timestamp() * 1e9` drifts by hundreds of ns and then no longer
    matches the leaderboard's own `ts`, which carries microseconds)."""
    return (t - _EPOCH) // timedelta(microseconds=1) * 1_000


# ── which accounts are rivals ──────────────────────────────────────────────────────────────


def _latest(leaderboard: pl.DataFrame) -> pl.DataFrame:
    return leaderboard.filter(pl.col("ts") == pl.col("ts").max())


def our_rank(leaderboard: pl.DataFrame) -> int | None:
    """Our rank in the newest snapshot (the leaderboard's `my_rank`), if recorded."""
    if leaderboard.is_empty() or "my_rank" not in leaderboard.columns:
        return None
    ranks = _latest(leaderboard)["my_rank"].drop_nulls()
    return int(ranks[0]) if ranks.len() else None


def select_rivals(leaderboard: pl.DataFrame, n: int) -> list[str]:
    """Profile ids of the `n` best marks in the newest snapshot, excluding our own row.

    Ids stay inside this process: callers turn them into slots before anything is written."""
    if leaderboard.is_empty():
        return []
    latest = _latest(leaderboard)
    mine = our_rank(leaderboard)
    if mine is not None and "rank" in latest.columns:
        latest = latest.filter(pl.col("rank") != mine)
    return latest.sort(["pnl", "profile_id"], descending=[True, False])["profile_id"].head(n).to_list()


# ── the fit ────────────────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class PositionFit:
    positions: dict[str, float]  # exchange id -> signed shares (+YES, −NO)
    r2: float
    windows: int
    trimmed: int


def _omp(x: FloatArray, y: FloatArray, kmax: int) -> tuple[list[int], FloatArray]:
    """Orthogonal matching pursuit with a BIC stop: (selected columns, coefficients)."""
    n = y.shape[0]
    norms = np.linalg.norm(x, axis=0)
    usable = norms > 0
    sel: list[int] = []
    resid = y.copy()
    rss0 = float(resid @ resid)
    best_bic = n * math.log(rss0 / n + 1e-12)
    best: tuple[list[int], FloatArray] = ([], np.zeros(0))
    for _ in range(min(kmax, int(usable.sum()), max(0, n - 2))):
        score = np.where(usable, np.abs(x.T @ resid) / np.where(usable, norms, 1.0), -1.0)
        score[sel] = -1.0
        j = int(np.argmax(score))
        if score[j] <= 0:
            break
        sel.append(j)
        coef, *_ = np.linalg.lstsq(x[:, sel], y, rcond=None)
        resid = y - x[:, sel] @ coef
        bic = n * math.log(float(resid @ resid) / n + 1e-12) + len(sel) * math.log(n)
        if bic < best_bic:
            best_bic, best = bic, (list(sel), coef.copy())
    return best


def fit_positions(
    d_values: FloatArray,
    d_pnl: FloatArray,
    age_h: FloatArray,
    exchanges: Sequence[str],
    cfg: FitConfig,
) -> PositionFit:
    """Signed shares per exchange from quiet windows (rows of `d_values`: Δprice per exchange)."""
    n = d_pnl.shape[0]
    if n < cfg.min_windows:
        return PositionFit({}, 0.0, n, 0)
    moved = np.flatnonzero(np.abs(d_values).max(axis=0) > MIN_PRICE_MOVE)
    w = np.sqrt(0.5 ** (age_h / cfg.half_life_h))
    keep = np.ones(n, dtype=bool)
    trimmed = 0
    sel: list[int] = []
    coef = np.zeros(0)
    for attempt in range(2):
        x = d_values[keep][:, moved] * w[keep, None]
        y = d_pnl[keep] * w[keep]
        # at most one position per four windows: a fit with as many legs as windows explains anything
        sel, coef = _omp(x, y, min(cfg.max_positions, int(keep.sum()) // 4))
        if attempt == 1:
            break
        fitted = d_values[:, moved][:, sel] @ coef if sel else np.zeros(n)
        resid = d_pnl - fitted
        mad = float(np.median(np.abs(resid - np.median(resid)))) or 0.0
        out = np.abs(resid) > TRIM_MADS * max(mad, 1e-9) if mad > 0 else np.zeros(n, dtype=bool)
        if not out.any() or (~out).sum() < cfg.min_windows:
            break
        keep, trimmed = ~out, int(out.sum())
    y_all = d_pnl[keep] * w[keep]
    x_all = d_values[keep][:, moved] * w[keep, None]
    fitted = x_all[:, sel] @ coef if sel else np.zeros_like(y_all)
    ss_tot = float(((y_all - y_all.mean()) ** 2).sum())
    r2 = 1.0 - float(((y_all - fitted) ** 2).sum()) / ss_tot if ss_tot > 0 else 0.0
    positions = {exchanges[int(moved[j])]: float(c) for j, c in zip(sel, coef, strict=True)}
    return PositionFit(positions, max(0.0, r2), int(keep.sum()), trimmed)


def side_price(q: float, v: float) -> float:
    """What one share of the held side is marked at: v for YES (q > 0), 1 − v for NO."""
    return v if q > 0 else 1.0 - v


@dataclass
class RivalBook:
    """One rival's inferred book (no identity: `slot` is the rank among rivals by current mark)."""

    slot: str
    balance: float  # current mark: 100,000 + P&L
    positions: dict[str, float]  # exchange id -> signed shares
    r2: float
    windows: int
    hours_since_trade: float | None
    modeled: bool
    reason: str = ""

    @property
    def mark_multiple(self) -> float:
        return self.balance / INITIAL_BALANCE

    def gross(self, marks: Mapping[str, float]) -> float:
        return sum(abs(q) * side_price(q, marks.get(ex, 0.5)) for ex, q in self.positions.items())


def infer_books(
    leaderboard: pl.DataFrame, panel: PricePanel, rivals: Sequence[str], cfg: FitConfig
) -> list[RivalBook]:
    """A `RivalBook` per profile id in `rivals` (slots R01, R02, ... in the given order)."""
    ts_ns = panel.ts_ns
    if ts_ns.size < 2:
        return [RivalBook(_slot(i), INITIAL_BALANCE, {}, 0.0, 0, None, False, "no snapshots")
                for i, _ in enumerate(rivals)]  # fmt: skip
    index = {int(t): k for k, t in enumerate(ts_ns)}
    values = np.nan_to_num(panel.values, nan=0.0)
    filled = np.isfinite(panel.values)
    d_values = np.diff(values, axis=0)
    d_values[~(filled[1:] & filled[:-1])] = 0.0  # an exchange's first price is not a move
    marks = panel.latest()
    end_ns = int(ts_ns[-1])
    lookback_ns = cfg.lookback_h * NS_PER_HOUR
    lb = leaderboard.select("ts", "profile_id", "pnl", "trades_count").with_columns(
        pl.col("ts").dt.epoch("ns").alias("ts_ns")
    )
    books: list[RivalBook] = []
    for i, pid in enumerate(rivals):
        rows = lb.filter(pl.col("profile_id") == pid).sort("ts_ns")
        pnl = np.full(ts_ns.size, np.nan)
        trades = np.full(ts_ns.size, np.nan)
        for t, p, c in rows.select("ts_ns", "pnl", "trades_count").iter_rows():
            k = index.get(int(t))
            if k is not None and p is not None and c is not None:
                pnl[k], trades[k] = p, c
        balance = INITIAL_BALANCE + float(rows["pnl"][-1]) if rows.height else INITIAL_BALANCE
        d_pnl, d_trades = np.diff(pnl), np.diff(trades)
        quiet = np.isfinite(d_pnl) & (d_trades == 0) & (ts_ns[1:] >= end_ns - lookback_ns)
        age_h = (end_ns - ts_ns[1:][quiet]) / NS_PER_HOUR
        fit = fit_positions(d_values[quiet], d_pnl[quiet], age_h, panel.exchanges, cfg)
        positions = {
            ex: q for ex, q in fit.positions.items()
            if abs(q) * side_price(q, marks.get(ex, 0.5)) >= cfg.min_position_frac * balance
        }  # fmt: skip
        active = np.flatnonzero(np.isfinite(d_trades) & (d_trades > 0))
        since = (end_ns - int(ts_ns[active[-1] + 1])) / NS_PER_HOUR if active.size else None
        book = RivalBook(_slot(i), balance, positions, fit.r2, fit.windows, since, False)
        gross = book.gross(marks)
        if fit.windows < cfg.min_windows:
            book.reason = f"{fit.windows} quiet windows < {cfg.min_windows}"
        elif fit.r2 < cfg.min_r2:
            book.reason = f"fit R2 {fit.r2:.2f} < {cfg.min_r2:.2f}"
        elif gross > cfg.max_gross_frac * max(balance, 1.0):
            book.reason = f"implausible book (gross {gross / max(balance, 1.0):.1f}x balance)"
        else:
            book.modeled = True
        books.append(book)
    return books


def _slot(i: int) -> str:
    return f"R{i + 1:02d}"


# ── outcomes and payout vectors ────────────────────────────────────────────────────────────


class Outcomes:
    """YES outcome per world for any exchange: simulated where the map and the sim cover it, else
    an independent Bernoulli(mark) draw, one per exchange and shared by every book that holds it."""

    def __init__(self, columns: Mapping[str, BoolArray], n: int, *, seed: int) -> None:
        self.columns = dict(columns)
        self.n = n
        self.seed = seed
        self._drawn: dict[str, BoolArray] = {}

    def yes(self, exchange_id: str, mark: float) -> BoolArray:
        col = self.columns.get(exchange_id)
        if col is not None:
            return col
        if exchange_id not in self._drawn:
            key = int(hashlib.sha256(f"{self.seed}/{exchange_id}".encode()).hexdigest()[:16], 16)
            self._drawn[exchange_id] = np.random.default_rng(key).random(self.n) < mark
        return self._drawn[exchange_id]

    def simulated(self, exchange_id: str) -> bool:
        return exchange_id in self.columns


def outcome_columns(
    draws: SimDraws, specs: Sequence[RaceSpec], rules: Sequence[ops_pb2.MarketRule]
) -> dict[str, BoolArray]:
    """exchange id -> YES outcome per world for every mapped exchange the simulator covers.

    Reuses `leg_outcomes` (the optimizer's own payoff map) on placeholder legs, so a rival's contract
    pays in exactly the worlds where the same leg would pay for us."""
    legs = []
    for r in rules:
        if not r.race_id or not r.party:
            continue
        state = next((c.split(":", 1)[1] for c in r.clusters if c.startswith("state:")), "")
        legs.append(Leg(r.exchange_id, r.market_id, r.title, r.race_id, r.party, 0.5, None, 0.5, 0.5,
                        tuple(r.clusters), state, r.kind, r.verified))  # fmt: skip
    matrix, kept = leg_outcomes(draws, specs, legs)
    return {leg.exchange_id: matrix[:, j] for j, leg in enumerate(kept)}


def payout_multiple(
    balance: float, positions: Mapping[str, float], marks: Mapping[str, float], outcomes: Outcomes
) -> FloatArray:
    """Resolved balance / 100,000 per world: (balance + Σ q · (Y − v)) floored at 0."""
    final = np.full(outcomes.n, float(balance))
    for ex, q in positions.items():
        v = float(marks.get(ex, 0.5))
        final += q * (outcomes.yes(ex, v).astype(np.float64) - v)
    return np.asarray(np.maximum(final, 0.0) / INITIAL_BALANCE, dtype=np.float64)


def unexplained_value(balance: float, positions: Mapping[str, float], marks: Mapping[str, float]) -> float:
    """Balance the fitted positions do not account for (cash plus positions the fit cannot see), >= 0."""
    gross = sum(abs(q) * side_price(q, marks.get(ex, 0.5)) for ex, q in positions.items())
    return max(0.0, balance - gross)


def payout_multiple_low(
    balance: float, positions: Mapping[str, float], marks: Mapping[str, float], outcomes: Outcomes
) -> FloatArray:
    """Lower bound of `payout_multiple`: the unexplained value resolves to zero (a dead mark, not cash)."""
    low = balance - unexplained_value(balance, positions, marks)
    return payout_multiple(low, positions, marks, outcomes)


def rival_matrix(books: Sequence[RivalBook], marks: Mapping[str, float], outcomes: Outcomes) -> FloatArray:
    """(worlds × modeled rivals) resolved multiples, in slot order."""
    cols = [payout_multiple(b.balance, b.positions, marks, outcomes) for b in books if b.modeled]
    return np.column_stack(cols) if cols else np.zeros((outcomes.n, 0))


def rival_threshold(matrix: FloatArray) -> FloatArray:
    """R(ω): the best resolved multiple among the modeled rivals, floored at 1.0 like the fitted T (a
    threshold below the starting balance would make the optimizer's closeness measure divide by zero)."""
    if matrix.shape[1] == 0:
        return np.ones(matrix.shape[0])
    return np.asarray(np.maximum(matrix.max(axis=1), 1.0), dtype=np.float64)


def blend_threshold(threshold: FloatArray, rival: FloatArray, weight: float) -> FloatArray:
    """T_w = (1 − w) · T + w · R; w = 0 returns `threshold` itself (the hook is off)."""
    if weight <= 0.0:
        return threshold
    w = min(1.0, float(weight))
    return np.asarray((1.0 - w) * threshold + w * rival, dtype=np.float64)


# ── the book file the optimizer hook reads (on-box only; slots, never accounts) ────────────


@dataclass
class RivalBookFile:
    produced_at_ns: int
    marks: dict[str, float]
    books: list[RivalBook] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        return {"produced_at_ns": self.produced_at_ns, "marks": self.marks,
                "books": [asdict(b) for b in self.books]}  # fmt: skip

    @classmethod
    def from_dict(cls, raw: Mapping[str, Any]) -> RivalBookFile:
        return cls(
            int(raw["produced_at_ns"]),
            {str(k): float(v) for k, v in (raw.get("marks") or {}).items()},
            [RivalBook(**b) for b in raw.get("books") or []],
        )


# ── analysis (aggregates only) ─────────────────────────────────────────────────────────────


def _q(x: FloatArray, qs: Sequence[float] = (0.1, 0.5, 0.9)) -> list[float]:
    return [round(float(v), 3) for v in np.quantile(x, qs)] if x.size else []


def _corr(a: FloatArray, b: FloatArray) -> float:
    if a.std() == 0 or b.std() == 0:
        return 0.0
    return float(np.corrcoef(a, b)[0, 1])


@dataclass
class Exposure:
    title: str
    side: str  # YES | NO
    face_multiple: float  # what the holding pays, as a multiple of the starting balance
    mark_share: float  # marked value / rival balance
    p_pays: float
    longshot: bool
    simulated: bool
    ours: str  # "same side" | "opposite side" | ""


@dataclass
class RivalStats:
    slot: str
    modeled: bool
    reason: str
    mark_multiple: float
    r2: float
    windows: int
    positions: int
    hours_since_trade: float | None
    longshot_mark_share: float  # share of the balance marked in longshot holdings
    unexplained_share: float  # share of the balance the fit does not see (cash or static marks)
    resolved_mean: float | None = None
    resolved_q: list[float] = field(default_factory=list)  # p10 / p50 / p90, unexplained value at mark
    resolved_low_q: list[float] = field(default_factory=list)  # unexplained value resolving to zero
    p_below_half_mark: float | None = None
    p_beats_us: float | None = None
    corr_with_us: float | None = None
    lift_in_our_scenario: float | None = None  # E[M | our scenario] / E[M]
    exposures: list[Exposure] = field(default_factory=list)


def _exposures(
    book: RivalBook,
    marks: Mapping[str, float],
    outcomes: Outcomes,
    titles: Mapping[str, str],
    ours: Mapping[str, float],
    top: int,
) -> list[Exposure]:
    out = []
    for ex, q in book.positions.items():
        v = float(marks.get(ex, 0.5))
        y = outcomes.yes(ex, v)
        p_yes = float(y.mean())
        mine = ours.get(ex, 0.0)
        overlap = "" if mine == 0 else ("same side" if (mine > 0) == (q > 0) else "opposite side")
        out.append(
            Exposure(
                title=titles.get(ex, f"exchange {ex}"),
                side="YES" if q > 0 else "NO",
                face_multiple=round(abs(q) / INITIAL_BALANCE, 3),
                mark_share=round(abs(q) * side_price(q, v) / max(book.balance, 1.0), 3),
                p_pays=round(p_yes if q > 0 else 1.0 - p_yes, 3),
                longshot=side_price(q, v) <= LONGSHOT_MAX,
                simulated=outcomes.simulated(ex),
                ours=overlap,
            )
        )
    out.sort(key=lambda e: -e.face_multiple * (1.0 - min(e.p_pays, 1.0) + 1e-3))
    return out[:top]


def longshot_share(book: RivalBook, marks: Mapping[str, float]) -> float:
    held = sum(
        abs(q) * side_price(q, marks.get(ex, 0.5))
        for ex, q in book.positions.items()
        if side_price(q, marks.get(ex, 0.5)) <= LONGSHOT_MAX
    )
    return held / max(book.balance, 1.0)


@dataclass
class RivalReport:
    """Everything the report shows: aggregates per slot and over the field, no identities."""

    produced_at_ns: int
    worlds: int
    focus_n: int
    wide_n: int
    our_scenario: str
    our_mark_multiple: float
    our_resolved_q: list[float]
    modeled_focus: int
    modeled_wide: int
    rivals: list[RivalStats]
    field_threshold_q: list[float]  # the tape-fitted / fallback T the optimizer uses
    rival_threshold_focus_q: list[float]
    rival_threshold_wide_q: list[float]
    true_threshold_q: list[float]  # max(T, R_wide): the field maximum once the leaders' marks resolve
    rival_threshold_wide_low_q: list[float]  # R_wide with every rival's unexplained value at zero
    true_threshold_low_q: list[float]  # max(T, R_wide_low)
    p_beat_focus: float
    p_beat_wide: float
    p_beat_field: float
    p_beat_true: float
    positively_correlated: int  # wide set, corr > CORR_POSITIVE
    strongly_correlated: int  # corr > CORR_STRONG
    exposed_to_our_scenario: int  # lift > SCENARIO_LIFT and corr > CORR_POSITIVE
    exposed_to_our_scenario_focus: int
    cash_floor: list[dict[str, Any]]  # place k: the multiple that beats all but k−1 modeled rivals
    notes: list[str] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        return asdict(self)


CORR_POSITIVE = 0.1
CORR_STRONG = 0.3
SCENARIO_LIFT = 1.1


def analyze(
    books: Sequence[RivalBook],
    marks: Mapping[str, float],
    outcomes: Outcomes,
    ours: FloatArray,
    our_positions: Mapping[str, float],
    field_threshold: FloatArray,
    *,
    our_scenario: str = "",
    scenario_mask: BoolArray | None = None,
    our_mark_multiple: float = 1.0,
    titles: Mapping[str, str] | None = None,
    focus_n: int = 5,
    produced_at_ns: int = 0,
    top_exposures: int = 5,
) -> RivalReport:
    """Per-slot and field aggregates for the report (books in slot order, focus = the first `focus_n`)."""
    titles = titles or {}
    stats: list[RivalStats] = []
    resolved: dict[str, FloatArray] = {}
    resolved_low: dict[str, FloatArray] = {}
    for b in books:
        s = RivalStats(
            b.slot, b.modeled, b.reason, round(b.mark_multiple, 3), round(b.r2, 3), b.windows,
            len(b.positions), None if b.hours_since_trade is None else round(b.hours_since_trade, 1),
            round(longshot_share(b, marks), 3),
            round(unexplained_value(b.balance, b.positions, marks) / max(b.balance, 1.0), 3),
        )  # fmt: skip
        if b.modeled:
            m = payout_multiple(b.balance, b.positions, marks, outcomes)
            resolved[b.slot] = m
            resolved_low[b.slot] = payout_multiple_low(b.balance, b.positions, marks, outcomes)
            s.resolved_low_q = _q(resolved_low[b.slot])
            s.resolved_mean = round(float(m.mean()), 3)
            s.resolved_q = _q(m)
            s.p_below_half_mark = round(float((m < 0.5 * b.mark_multiple).mean()), 3)
            s.p_beats_us = round(float((m > ours).mean()), 3)
            s.corr_with_us = round(_corr(m, ours), 3)
            if scenario_mask is not None and scenario_mask.any() and m.mean() > 0:
                s.lift_in_our_scenario = round(float(m[scenario_mask].mean() / m.mean()), 3)
            s.exposures = _exposures(b, marks, outcomes, titles, our_positions, top_exposures)
        stats.append(s)

    focus_slots = [b.slot for b in books[:focus_n] if b.modeled]
    wide = np.column_stack(list(resolved.values())) if resolved else np.zeros((outcomes.n, 0))
    focus = np.column_stack([resolved[s] for s in focus_slots]) if focus_slots else np.zeros((outcomes.n, 0))
    r_focus, r_wide = rival_threshold(focus), rival_threshold(wide)
    true_t = np.maximum(field_threshold, r_wide)
    wide_low = np.column_stack(list(resolved_low.values())) if resolved_low else np.zeros((outcomes.n, 0))
    r_wide_low = rival_threshold(wide_low)
    true_t_low = np.maximum(field_threshold, r_wide_low)

    def exposed(rows: Sequence[RivalStats]) -> int:
        return sum(
            1
            for s in rows
            if s.modeled
            and (s.corr_with_us or 0) > CORR_POSITIVE
            and (s.lift_in_our_scenario or 0) > SCENARIO_LIFT
        )

    modeled = [s for s in stats if s.modeled]
    floor = []
    if wide.shape[1]:
        ordered = -np.sort(-wide, axis=1)
        for k in range(1, min(3, wide.shape[1]) + 1):
            kth = ordered[:, k - 1]
            floor.append({
                "place": k, "beat_all_but": k - 1, "multiple_p50": round(float(np.quantile(kth, 0.5)), 3),
                "multiple_p90": round(float(np.quantile(kth, 0.9)), 3),
                "our_p_reach": round(float((ours > kth).mean()), 3),
            })  # fmt: skip
    notes = []
    unmodeled = [s for s in stats if not s.modeled]
    if unmodeled:
        notes.append(
            f"{len(unmodeled)} of {len(stats)} rivals not modeled (thin or poor fit); they are left out of R"
        )
    sim_share = _simulated_share(books, marks, outcomes)
    notes.append(f"{sim_share:.0%} of the modeled rivals' gross exposure is on simulated contracts")
    return RivalReport(
        produced_at_ns=produced_at_ns,
        worlds=outcomes.n,
        focus_n=focus_n,
        wide_n=len(books),
        our_scenario=our_scenario,
        our_mark_multiple=round(our_mark_multiple, 3),
        our_resolved_q=_q(ours),
        modeled_focus=len(focus_slots),
        modeled_wide=len(modeled),
        rivals=stats,
        field_threshold_q=_q(field_threshold),
        rival_threshold_focus_q=_q(r_focus),
        rival_threshold_wide_q=_q(r_wide),
        true_threshold_q=_q(true_t),
        rival_threshold_wide_low_q=_q(r_wide_low),
        true_threshold_low_q=_q(true_t_low),
        p_beat_focus=round(float((ours > r_focus).mean()), 4),
        p_beat_wide=round(float((ours > r_wide).mean()), 4),
        p_beat_field=round(float((ours > field_threshold).mean()), 4),
        p_beat_true=round(float((ours > true_t).mean()), 4),
        positively_correlated=sum(1 for s in modeled if (s.corr_with_us or 0) > CORR_POSITIVE),
        strongly_correlated=sum(1 for s in modeled if (s.corr_with_us or 0) > CORR_STRONG),
        exposed_to_our_scenario=exposed(stats),
        exposed_to_our_scenario_focus=exposed(stats[:focus_n]),
        cash_floor=floor,
        notes=notes,
    )


def _simulated_share(books: Sequence[RivalBook], marks: Mapping[str, float], outcomes: Outcomes) -> float:
    total = sim = 0.0
    for b in books:
        if not b.modeled:
            continue
        for ex, q in b.positions.items():
            g = abs(q) * side_price(q, marks.get(ex, 0.5))
            total += g
            sim += g if outcomes.simulated(ex) else 0.0
    return sim / total if total else 0.0


def render_markdown(r: RivalReport) -> str:
    """The human report: aggregates per slot, the field threshold, the cash floor."""

    def q(v: Sequence[float]) -> str:
        return " / ".join(f"{x:.2f}" for x in v) if v else "n/a"

    lines = [
        "# Endgame rival model (EXP-011, draft)",
        "",
        f"Worlds {r.worlds:,}; rivals: top {r.focus_n} (focus) and top {r.wide_n} (wide) by current mark, "
        f"excluding us; modeled {r.modeled_focus}/{r.focus_n} and {r.modeled_wide}/{r.wide_n}. "
        "Rivals are slots by current mark; no account is named.",
        "",
        "## Us",
        f"- Scenario: {r.our_scenario or 'none'}; mark {r.our_mark_multiple:.2f}x; resolved p10/p50/p90 "
        f"{q(r.our_resolved_q)}x",
        f"- P(beat every modeled focus rival) {r.p_beat_focus:.3f}; wide {r.p_beat_wide:.3f}; "
        f"P(M > fitted T) {r.p_beat_field:.3f}; P(M > max(T, R_wide)) {r.p_beat_true:.3f}",
        "",
        "## Field threshold once the leaders' marks resolve (p10 / p50 / p90, multiple)",
        f"- Fitted T (what the optimizer uses today): {q(r.field_threshold_q)}",
        f"- R, best modeled focus rival: {q(r.rival_threshold_focus_q)}",
        f"- R, best modeled wide rival: {q(r.rival_threshold_wide_q)}",
        f"- True T estimate, max(T, R_wide): {q(r.true_threshold_q)}",
        f"- Lower bound, unexplained value resolving to zero: R_wide {q(r.rival_threshold_wide_low_q)}; "
        f"max(T, R_wide) {q(r.true_threshold_low_q)}",
        "",
        "## Correlation with our book",
        f"- Modeled rivals positively correlated with us (corr > {CORR_POSITIVE}): "
        f"{r.positively_correlated}; strongly (> {CORR_STRONG}): {r.strongly_correlated}",
        f"- Exposed to our scenario (corr > {CORR_POSITIVE} and lift > {SCENARIO_LIFT} in it): "
        f"{r.exposed_to_our_scenario} (focus {r.exposed_to_our_scenario_focus})",
        "",
        "| Slot | Mark | Modeled | R2 | Windows | Legs | Longshot share | Unexplained share | "
        "Resolved p10/p50/p90 | Low bound p10/p50/p90 | P(< half mark) | P(beats us) | Corr | "
        "Lift in our S |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for s in r.rivals:
        lines.append(
            f"| {s.slot} | {s.mark_multiple:.2f}x | {'yes' if s.modeled else 'no: ' + s.reason} | "
            f"{s.r2:.2f} | {s.windows} | {s.positions} | {s.longshot_mark_share:.2f} | "
            f"{s.unexplained_share:.2f} | "
            f"{q(s.resolved_q)} | {q(s.resolved_low_q)} | "
            f"{_f(s.p_below_half_mark)} | {_f(s.p_beats_us)} | {_f(s.corr_with_us)} | "
            f"{_f(s.lift_in_our_scenario)} |"
        )
    lines += ["", "## Where the focus rivals are most exposed", ""]
    for s in r.rivals[: r.focus_n]:
        if not s.exposures:
            continue
        lines.append(f"**{s.slot}**")
        for e in s.exposures:
            tag = ", longshot" if e.longshot else ""
            mine = f", ours: {e.ours}" if e.ours else ""
            sim = "" if e.simulated else ", not simulated"
            lines.append(
                f"- {e.title} {e.side}: pays {e.face_multiple:.2f}x of 100k with P {e.p_pays:.2f}; "
                f"{e.mark_share:.0%} of its mark{tag}{sim}{mine}"
            )
        lines.append("")
    lines += ["## Cash floor that locks a placing (modeled rivals only)", ""]
    if r.cash_floor:
        lines += ["| Place | Multiple p50 | Multiple p90 | Our P(reach) |", "|---|---|---|---|"]
        lines += [
            f"| {c['place']} | {c['multiple_p50']:.2f}x | {c['multiple_p90']:.2f}x | {c['our_p_reach']:.3f} |"
            for c in r.cash_floor
        ]
    else:
        lines.append("No modeled rival.")
    lines += ["", "## Notes", *[f"- {n}" for n in r.notes], ""]
    return "\n".join(lines)


def _f(v: float | None) -> str:
    return "n/a" if v is None else f"{v:.2f}"


def snapshot_times(leaderboard: pl.DataFrame, *, since: datetime | None = None) -> list[datetime]:
    if leaderboard.is_empty():
        return []
    ts = leaderboard["ts"].unique().sort()
    out = ts.to_list()
    return [t for t in out if since is None or t >= since]
