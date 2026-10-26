"""True cash and risk-cap use from the trader's own portfolio reads, and scenario-order refusals.

`status.json` cash used to be the start-up `myBalance` (EXP-015), and the packet had no cap-use
figures, so six days of scenario orders refused on caps went unflagged. Everything here is
recomputed from the day's journal:

* **Cash** is `totalAccountValue - totalHoldingsValue` from the last `GET .../portfolio/pnl`
  response, the figure the bridge's risk engine sizes against (`gateway.portfolio`).
* **Cap use** repeats the bridge's exposure arithmetic (`risk.py`) on the last positions read and
  the last complete open-orders scan: positions at mark plus resting orders at their limit, as a
  fraction of account value (`total_gross`) or of each cluster's cap.
* **Scenario refusals** are the bridge's `scenario_order` events refused on a cap, plus, once
  rust-live is the live trader, its `blocked` events for scenario bundles.

Only aggregates leave this module (cluster labels such as `region:south` are not identifiers).
"""

from __future__ import annotations

import json
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any
from urllib.parse import parse_qs, urlsplit

from susq.contracts import ops_pb2

SCENARIO = "scenario"
RUST_LIVE = "rust-live"
# rust-live's `Rejection` messages for the exposure caps (`risk: per-market cap on ...`, ...).
_RUST_CAPS = ("per-market cap", "cluster cap", "gross cap")


@dataclass
class CapUse:
    equity: float | None = None
    cash: float | None = None
    #: Positions at mark plus resting orders, SUSQies (what `total_gross` caps).
    total_gross: float | None = None
    #: (cluster, used SUSQies, cap SUSQies), highest use-of-cap first.
    clusters: list[tuple[str, float, float]] = field(default_factory=list)

    @property
    def cash_frac(self) -> float | None:
        return self.cash / self.equity if self.cash is not None and self.equity else None

    @property
    def total_gross_frac(self) -> float | None:
        return self.total_gross / self.equity if self.total_gross is not None and self.equity else None

    def top_clusters(self, n: int = 3) -> list[tuple[str, float]]:
        """The `n` clusters closest to their caps: (cluster, used / cap)."""
        return [(c, used / cap) for c, used, cap in self.clusters[:n]]


def _body(entry: ops_pb2.JournalEntry) -> Any:
    try:
        return json.loads(entry.body_json) if entry.body_json else None
    except json.JSONDecodeError:
        return None


def _ok_get(entry: ops_pb2.JournalEntry) -> bool:
    return entry.direction == "response" and entry.method == "GET" and 200 <= entry.status < 300


def _query(path: str) -> tuple[str, dict[str, list[str]]]:
    parts = urlsplit(path)
    return parts.path, parse_qs(parts.query)


def _is_unfiltered_open_orders(path: str) -> tuple[bool, bool]:
    """(is an unfiltered open-orders page, is a follow-on page with a cursor)."""
    route, q = _query(path)
    if route.rstrip("/") != "/orders" or q.get("status", ["open"])[0] != "open":
        return False, False
    if "exchangeId" in q or "marketId" in q:
        return False, False
    return True, "cursor" in q


@dataclass
class _Reads:
    pnl: dict[str, Any] | None = None
    positions: list[dict[str, Any]] | None = None
    open_orders: list[dict[str, Any]] | None = None


def last_reads(entries: Iterable[ops_pb2.JournalEntry]) -> _Reads:
    """The last pnl and positions responses and the last complete unfiltered open-orders scan."""
    reads = _Reads()
    scan: list[dict[str, Any]] | None = None
    for e in entries:
        if not _ok_get(e):
            continue
        route, _ = _query(e.path)
        body = _body(e)
        if not isinstance(body, dict):
            continue
        if route.endswith("/portfolio/pnl"):
            reads.pnl = body
        elif route.endswith("/portfolio/positions"):
            reads.positions = [p for p in body.get("positions") or [] if isinstance(p, dict)]
        else:
            is_orders, follow_on = _is_unfiltered_open_orders(e.path)
            if not is_orders:
                continue
            page = [o for o in body.get("data") or [] if isinstance(o, dict)]
            if not follow_on:
                scan = page
            elif scan is not None:
                scan += page
            else:
                continue  # a follow-on page whose first page was on an earlier day
            pagination = body.get("pagination") or {}
            if not (pagination.get("hasMore") and pagination.get("nextCursor")):
                reads.open_orders, scan = scan, None
    return reads


def _num(value: Any) -> float | None:
    try:
        out = float(value)
    except (TypeError, ValueError):
        return None
    return out if out == out and abs(out) != float("inf") else None


def _exposures(
    positions: Sequence[Mapping[str, Any]], open_orders: Sequence[Mapping[str, Any]]
) -> dict[str, float]:
    """Per-exchange exposure, SUSQies: `risk.market_exposure_pips` in units.

    A position is valued at its side's mark (`currentPrice` is the YES mark; no mark counts a full
    unit per share). A resting order counts its remaining shares at the cost of its YES-space side:
    a YES buy at its limit, anything else at one minus its YES-space limit.
    """
    out: dict[str, float] = {}
    for p in positions:
        qty = round(_num(p.get("quantity")) or 0)
        ex = str(p.get("exchangeId") or "")
        if not qty or not ex:
            continue
        mark = _num(p.get("currentPrice"))
        unit = 1.0 if mark is None else (mark if qty > 0 else 1.0 - mark)
        out[ex] = out.get(ex, 0.0) + abs(qty) * unit
    for o in open_orders:
        ex = str(o.get("exchangeId") or "")
        remaining = round(_num(o.get("quantity")) or 0)
        limit = _num(o.get("priceLimit"))
        if not ex or remaining <= 0 or limit is None:
            continue
        yes_dir = 1 if (o.get("side") == "yes") == (o.get("action") == "buy") else -1
        yes_price = limit if o.get("side") == "yes" else 1.0 - limit
        unit = yes_price if yes_dir > 0 else 1.0 - yes_price
        out[ex] = out.get(ex, 0.0) + remaining * unit
    return out


def cluster_cap_frac(
    cluster: str, kind_caps: Mapping[str, float], overrides: Mapping[str, float]
) -> float | None:
    """The tightest of the cluster kind's cap (`[risk.clusters]`) and the targets file's override."""
    caps = [c for c in (kind_caps.get(cluster.split(":", 1)[0]), overrides.get(cluster)) if c is not None]
    return min(caps) if caps else None


def cap_use(
    entries: Iterable[ops_pb2.JournalEntry],
    *,
    clusters_by_exchange: Mapping[str, Iterable[str]],
    kind_caps: Mapping[str, float],
    overrides: Mapping[str, float] | None = None,
) -> CapUse:
    """Cash and cap use at the day's last portfolio reads (fields stay None when a read is missing)."""
    reads = last_reads(entries)
    use = CapUse()
    if reads.pnl is not None:
        use.equity = _num(reads.pnl.get("totalAccountValue"))
        holdings = _num(reads.pnl.get("totalHoldingsValue"))
        if use.equity is not None and holdings is not None:
            use.cash = use.equity - holdings
    if reads.positions is None or reads.open_orders is None or not use.equity:
        return use
    exposure = _exposures(reads.positions, reads.open_orders)
    use.total_gross = sum(exposure.values())
    used: dict[str, float] = {}
    for ex, value in exposure.items():
        for cluster in clusters_by_exchange.get(ex, ()):
            used[cluster] = used.get(cluster, 0.0) + value
    rows = []
    for cluster, value in used.items():
        frac = cluster_cap_frac(cluster, kind_caps, overrides or {})
        if frac:
            rows.append((cluster, value, frac * use.equity))
    rows.sort(key=lambda r: (-r[1] / r[2], r[0]))
    use.clusters = rows
    return use


def _is_scenario_bundle(order: Any) -> bool:
    legs = order.get("legs") if isinstance(order, dict) else None
    return bool(legs) and any(isinstance(leg, dict) and leg.get("strategy") == SCENARIO for leg in legs)


def scenario_refusals(
    entries: Iterable[ops_pb2.JournalEntry], *, rust_live_trader: bool, rust_sent: int = 0
) -> tuple[int, int]:
    """(scenario orders decided, of which refused on an exposure cap) for the day.

    The bridge journals every scenario order as a `scenario_order` event; a cap refusal has status
    `risk_rejected` and a reason starting `cap:`. rust-live journals only refusals (`blocked`
    events), so its decided count is those plus the scenario intents it sent (`rust_sent`). Its
    events count only while it is the live trader: the paper run journals under the same producer.
    """
    orders = refused = 0
    for e in entries:
        if e.direction != "event":
            continue
        if e.method == "scenario_order":
            body = _body(e) or {}
            orders += 1
            refused += body.get("status") == "risk_rejected" and str(body.get("reason") or "").startswith(
                "cap:"
            )
        elif e.method == "blocked" and rust_live_trader and e.producer == RUST_LIVE:
            body = _body(e) or {}
            if not _is_scenario_bundle(body.get("order")):
                continue
            orders += 1
            reason = str(body.get("reason") or "")
            refused += reason.startswith("risk:") and any(cap in reason for cap in _RUST_CAPS)
    if rust_live_trader:
        orders += rust_sent
    return orders, refused
