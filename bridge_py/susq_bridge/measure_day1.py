"""Day-one measurement: verify our assumptions against the live platform, then apply SPEC §4.

Produces ``reports/DAY1.md`` (human) and ``reports/day1.json`` (machine). Every probe is a section with
a status: ``ok``, ``failed``, ``skipped`` (paper mode / no key / not probed by design) or ``pending``
(needs the first fill, which ``first_trade`` records later via :func:`record_first_fill`).

Write probes (live mode only, lease holder only, all 1 share, far from the market, cancelled at once):

* an intentionally **off-tick** order that must come back ``400 VALIDATION_ERROR``;
* a flat ``sell yes 1 @ 0.95``, to read the engine's canonical ``buy no 1 @ 0.05`` response, check it
  rests in ``GET /orders?status=open``, cancel it, and cancel it again (expect ``409``).

Reads (any mode, with a key): both hosts, the tournament, a gentle rate-limit ramp on ``GET /account``
(stops at the first 429), realtime connectivity, a spread/depth census over every market, the
leaderboard, and whether constraint ``observedPrices`` track last trades or book mids.

ALL collateral (``collateral`` section): compare the engine's ``all`` economics on a
relationship-linked fill with the shared model (:mod:`susq.platform.collateral`). It uses the racer's
fills from this process if there are any; otherwise, live only, a deliberate 1-share bundle buying NO
on the two cheapest members of a mutually exclusive relationship (it pays at least 1 whatever
happens, so it costs at most ``COLLATERAL_PROBE_MAX_LOSS`` more than it returns). It records whether
``all`` came back non-null, the savings, and ``/portfolio/collateral`` before and after. The decision
table says "set use_collateral_buying_power = true" only when every compared field matches the model
within ``COLLATERAL_TOLERANCE``; otherwise it explains the mismatch and keeps the flag off.
"""

from __future__ import annotations

import asyncio
import contextlib
import time
import uuid
from collections.abc import Callable, Iterable
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

import httpx
import orjson
from susq.platform.client import SuperMarketClient
from susq.platform.errors import ApiError, PlatformError
from susq.platform.models import Collateral, OrderBook, iso_utc
from susq.platform.prices import PIPS_PER_UNIT, TICK_PIPS, to_pips
from susq.platform.realtime import RealtimeClient

from susq_bridge.collateral import ObservedEconomics
from susq_bridge.config import BridgeConfig
from susq_bridge.gateway import OrderSpec

if TYPE_CHECKING:
    from susq_bridge.runtime import Bridge

__all__ = [
    "COLLATERAL_PROBE_MAX_LOSS",
    "COLLATERAL_TOLERANCE",
    "Day1Report",
    "Section",
    "decide",
    "load_report",
    "measure_day1",
    "record_first_fill",
    "write_report",
]


@dataclass
class Section:
    name: str
    status: str  # ok | failed | skipped | pending
    summary: str
    data: dict[str, Any] = field(default_factory=dict)


@dataclass
class Day1Report:
    generated_at: str
    mode: str
    sections: list[Section] = field(default_factory=list)
    recommendations: list[str] = field(default_factory=list)
    action_needed: list[str] = field(default_factory=list)

    def add(self, name: str, status: str, summary: str, **data: Any) -> Section:
        section = Section(name, status, summary, data)
        self.sections = [s for s in self.sections if s.name != name] + [section]
        return section

    def get(self, name: str) -> Section | None:
        return next((s for s in self.sections if s.name == name), None)


#: Largest engine-vs-model difference (SUSQies, per compared field) that still counts as a match.
COLLATERAL_TOLERANCE = 0.01
#: The deliberate collateral probe may cost at most this much more than its guaranteed payout of 1.
COLLATERAL_PROBE_MAX_LOSS = 0.03


def _pct(values: list[float], q: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, round(q * (len(ordered) - 1))))
    return ordered[idx]


# ── probes ──────────────────────────────────────────────────────────────────────────────────


async def probe_hosts(config: BridgeConfig, transport: httpx.AsyncBaseTransport | None) -> dict[str, Any]:
    results: dict[str, Any] = {}
    for label, url in (("primary", config.platform.base_url), ("alternate", config.platform.alt_base_url)):
        async with SuperMarketClient(url, config.api_key, transport=transport, timeout_s=10) as client:
            started = time.perf_counter()
            try:
                account = await client.get_account()
                results[label] = {
                    "url": url,
                    "ok": True,
                    "latencyMs": round((time.perf_counter() - started) * 1000, 1),
                    "profileId": account.id,
                }
            except PlatformError as err:
                results[label] = {"url": url, "ok": False, "error": str(err)}
    return results


async def probe_rate_limit(
    client: SuperMarketClient, *, steps: tuple[float, ...] = (2, 4, 6, 8, 10), seconds_per_step: float = 2.0
) -> dict[str, Any]:
    """Ramp GET /account; stop at the first 429 and record Retry-After. Never exceeds the last step."""
    achieved = 0.0
    for rps in steps:
        n = max(1, round(rps * seconds_per_step))
        started = time.perf_counter()
        for i in range(n):
            target = started + i / rps
            if (delay := target - time.perf_counter()) > 0:
                await asyncio.sleep(delay)
            try:
                await client.get_account()
            except ApiError as err:
                if err.status == 429:
                    return {
                        "firstLimitedAtRps": rps,
                        "retryAfterS": err.retry_after_s,
                        "sustainedRps": achieved,
                    }
                raise
        achieved = rps
    return {"firstLimitedAtRps": None, "retryAfterS": None, "sustainedRps": achieved}


async def probe_realtime(bridge: Bridge, timeout_s: float = 10.0) -> dict[str, Any]:
    token = await bridge.client.mint_realtime_token()
    minted = [token]

    async def provider():  # type: ignore[no-untyped-def]
        return minted.pop() if minted else await bridge.client.mint_realtime_token()

    rt = RealtimeClient(provider)
    tid = bridge.tournament.id
    for channel, event in (
        (f"tournament:{tid}", "market_batch"),
        (f"relationships:violations:{tid}", "violation"),
    ):
        rt.subscribe(channel, event, lambda payload, meta: None)
    started = time.perf_counter()
    task = asyncio.create_task(rt.run())
    try:
        await asyncio.wait_for(rt.connected.wait(), timeout=timeout_s)
        return {"joined": True, "joinMs": round((time.perf_counter() - started) * 1000, 1), "channels": 2}
    except TimeoutError:
        return {"joined": False, "error": rt.stats.last_error}
    finally:
        await rt.stop()
        task.cancel()
        with contextlib.suppress(asyncio.CancelledError, Exception):
            await task


async def census(bridge: Bridge) -> dict[str, Any]:
    """Spread and top-of-book depth across every market (the Sponsor's footprint)."""
    books: dict[str, OrderBook] = {}
    for ex in sorted(bridge.market.exchange_market):
        books[ex] = await bridge.market.book(ex, depth=5, max_age_s=0.0)
    spreads, depths, low_priced, two_sided = [], [], 0, 0
    for book in books.values():
        if book.bids and book.asks:
            two_sided += 1
            bid, ask = to_pips(book.bids[0].price), to_pips(book.asks[0].price)
            spreads.append((ask - bid) / TICK_PIPS)
            depths.append(min(book.bids[0].quantity, book.asks[0].quantity))
            if (bid + ask) / 2 <= 500:
                low_priced += 1
    n = len(books)
    return {
        "exchanges": n,
        "twoSidedFrac": round(two_sided / n, 3) if n else 0.0,
        "spreadTicks": {"p10": _pct(spreads, 0.1), "median": _pct(spreads, 0.5), "p90": _pct(spreads, 0.9)},
        "topDepth": {"p10": _pct(depths, 0.1), "median": _pct(depths, 0.5), "p90": _pct(depths, 0.9)},
        "lowPricedFrac": round(low_priced / n, 3) if n else 0.0,
    }


async def probe_constraint_prices(bridge: Bridge) -> dict[str, Any]:
    client = bridge.client
    tid = bridge.tournament.id
    res = await bridge.gateway.read(
        "constraints", lambda: client.get_constraints(tid, violations_only=False, min_violation=0.0)
    )
    await bridge.market.refresh_prices()
    match_last = match_mid = compared = 0
    for ev in res.data:
        for obs in ev.observed_prices or []:
            compared += 1
            p = to_pips(obs.price)
            snap = bridge.market.prices.get(obs.exchange_id)
            if snap is not None and snap.latest_price is not None and to_pips(snap.latest_price) == p:
                match_last += 1
            if (mid := bridge.market.mid(obs.exchange_id)) is not None and abs(mid - p) <= TICK_PIPS // 2:
                match_mid += 1
    return {
        "relationships": len(res.data),
        "violations": res.violations_count,
        "observedCompared": compared,
        "matchLastTrade": match_last,
        "matchBookMid": match_mid,
    }


async def probe_writes(bridge: Bridge, report: Day1Report) -> None:
    """Off-tick 400, NO canonicalization, open-order listing and cancel semantics (live only)."""
    gw = bridge.gateway
    if (why := await gw.gate()) is not None:
        report.add("tick", "skipped", f"write probes blocked: {why}")
        report.add("no_mechanics", "skipped", f"write probes blocked: {why}")
        return
    ex = _far_exchange(bridge)
    if ex is None:
        report.add("tick", "skipped", "no exchange with a best bid below 0.90 to probe safely")
        report.add("no_mechanics", "skipped", "no exchange with a best bid below 0.90 to probe safely")
        return
    expiration = gw.expiration_for(bridge.clock(), 60)
    if expiration is None:
        report.add("tick", "skipped", "time gate")
        return
    body = {
        "exchangeId": ex,
        "side": "yes",
        "action": "buy",
        "quantity": 1,
        "price": 0.0125,  # 125 pips: off the 50-pip tick, far below any market
        "expirationDate": iso_utc(expiration),
        "tournamentId": bridge.tournament.id,
    }
    key = str(uuid.uuid4())
    bridge.journal.event("day1_probe", {"probe": "off_tick", "body": body}, idempotency_key=key)
    try:
        placed = await bridge.client.place_order_body(body, key)
    except ApiError as err:
        ok = err.status == 400 and err.code == "VALIDATION_ERROR"
        report.add(
            "tick",
            "ok" if ok else "failed",
            f"off-tick 0.0125 -> {err.status} {err.code}; tick 0.005 "
            + ("confirmed" if ok else "NOT confirmed"),
            httpStatus=err.status,
            code=err.code,
            details=err.details,
        )
    else:
        # It should never be accepted; if it was, take it straight back out.
        if placed.order_id is not None:
            await gw.cancel(placed.order_id)
        report.add(
            "tick", "failed", "off-tick order was ACCEPTED; tick assumption wrong", orderId=placed.order_id
        )
        report.action_needed.append("Off-tick order accepted: re-check the tick size before trading")

    spec = OrderSpec(ex, "yes", "sell", 1, 9_500, ttl_s=60)
    outcome = await gw.place(f"day1:no_mechanics:{bridge.clock().date().isoformat()}", "day1", spec)
    if not outcome.placed or outcome.result is None:
        report.add("no_mechanics", "failed", f"probe order not placed: {outcome.status} {outcome.reason}")
        return
    res = outcome.result
    canonical = [res.action, res.side, res.price, res.quantity]
    opens = await gw.read(
        "open_orders", lambda: bridge.client.list_open_orders(bridge.tournament.id, exchange_id=ex)
    )
    listed = any(o.id == res.order_id for o in opens)
    cancelled = await gw.cancel(res.order_id or 0)
    second: str | None = None
    try:
        await bridge.client.cancel_order(res.order_id or 0)
        second = "200"
    except ApiError as err:
        second = f"{err.status} {err.code}"
    report.add(
        "no_mechanics",
        "ok" if canonical[:2] == ["buy", "no"] else "failed",
        f"flat 'sell yes 1 @ 0.95' came back as {canonical}; listed open: {listed}; "
        f"cancel ok: {cancelled}; second cancel: {second}",
        canonical=canonical,
        listedOpen=listed,
        cancelled=cancelled,
        secondCancel=second,
        terminalReasonCode=res.terminal_reason_code,
    )


def _collateral_rows(collateral: Collateral) -> list[dict[str, Any]]:
    return [
        {
            "componentId": c.component_id,
            "coveredExchangeIds": c.covered_exchange_ids,
            "relationshipIds": c.relationship_ids,
            "outstandingAdvance": c.outstanding_advance,
            "guaranteedPayoutFloor": c.guaranteed_payout_floor,
            "diagnostic": c.diagnostic,
        }
        for c in collateral.data
    ]


async def _collateral_probe_legs(bridge: Bridge) -> list[OrderSpec] | None:
    """1 share of NO on the two highest-bid members of the cheapest mutually exclusive relationship.

    NO on two members of an "at most one YES" set pays at least 1 in every outcome, so the probe's
    worst case is its cost minus 1, capped at ``COLLATERAL_PROBE_MAX_LOSS``.
    """
    best: tuple[int, list[OrderSpec]] | None = None
    for rel in bridge.racer.relationships.values():
        if rel.type != "mutually_exclusive" or rel.status not in (None, "active"):
            continue
        members = rel.exchange_ids("member") or rel.exchange_ids()
        if len(members) < 2:
            continue
        bids: list[tuple[int, str]] = []
        for ex in members:
            book = await bridge.market.book(ex, max_age_s=0.0, priority=True)
            if book.bids:
                bids.append((to_pips(book.bids[0].price), ex))
        if len(bids) < 2:
            continue
        (b1, e1), (b2, e2) = sorted(bids, reverse=True)[:2]
        cost = 2 * PIPS_PER_UNIT - b1 - b2
        if cost > PIPS_PER_UNIT + round(COLLATERAL_PROBE_MAX_LOSS * PIPS_PER_UNIT):
            continue
        legs = [OrderSpec(e, "no", "buy", 1, PIPS_PER_UNIT - b, ttl_s=60) for b, e in ((b1, e1), (b2, e2))]
        if best is None or cost < best[0]:
            best = (cost, legs)
    return best[1] if best else None


def _linked_fills(observed: Iterable[ObservedEconomics]) -> list[ObservedEconomics]:
    """Responses that actually filled on a relationship-linked exchange."""
    return [o for o in observed if o.component_id is not None and o.quantity_traded > 0]


async def probe_collateral(bridge: Bridge, report: Day1Report) -> None:
    """Engine ``all`` economics vs the model on a relationship-linked fill (see module docs)."""
    gw = bridge.gateway
    tracker = gw.collateral
    tid = bridge.tournament.id
    await bridge.racer.load_relationships()
    before = await gw.read("collateral", lambda: bridge.client.get_collateral(tid))
    tracker.observe_collateral(before)
    tracker.sync_positions(gw.risk.positions)
    linked = _linked_fills(tracker.observed)
    source, filled, with_all = "racer", len(linked) + tracker.linked_without_all, len(linked)
    if not linked:
        why = None
        if bridge.mode != "live":
            why = "paper mode: no live relationship-linked fill to compare"
        elif (gate := await gw.gate()) is not None:
            why = f"probe blocked: {gate}"
        legs = None if why else await _collateral_probe_legs(bridge)
        if why is None and legs is None:
            why = (
                "no mutually exclusive pair has NO+NO within "
                f"{COLLATERAL_PROBE_MAX_LOSS} of its guaranteed payout; the racer's first bundle will do"
            )
        if why is not None or legs is None:
            report.add(
                "collateral",
                "skipped",
                f"{why}; /portfolio/collateral lists {len(before.data)} component(s)",
                components=_collateral_rows(before),
            )
            return
        seen = len(tracker.observed)
        outcome = await gw.place_bundle(f"day1:collateral:{bridge.clock().date().isoformat()}", "day1", legs)
        for res in outcome.results:
            if res.open and res.order_id is not None:
                await gw.cancel(res.order_id)
        if not outcome.placed:
            report.add("collateral", "failed", f"probe bundle not placed: {outcome.status} {outcome.reason}")
            return
        source = "probe"
        filled = sum(1 for r in outcome.results if r.quantity_traded)
        with_all = sum(1 for r in outcome.results if r.quantity_traded and r.all is not None)
        linked = _linked_fills(list(tracker.observed)[seen:])
    after = await gw.read("collateral", lambda: bridge.client.get_collateral(tid))
    savings = round(sum(o.collateral_savings for o in linked), 6)
    mismatches = [m for o in linked for m in o.mismatches(COLLATERAL_TOLERANCE)]
    if not linked:
        verdict = f"`all` came back null on {filled} relationship-linked fill(s): no relationship quote"
    elif mismatches:
        verdict = "engine differs from the model: " + "; ".join(mismatches)
    else:
        verdict = f"all {len(linked)} fill(s) match the model within {COLLATERAL_TOLERANCE}"
    matches = bool(linked) and not mismatches and with_all == filled
    report.add(
        "collateral",
        "ok" if matches else "failed",
        f"{source}: `all` non-null on {with_all}/{filled} fills, savings {savings}; {verdict}; "
        f"/portfolio/collateral {len(before.data)} -> {len(after.data)} component(s)",
        source=source,
        filled=filled,
        allNonNull=with_all,
        savings=savings,
        matches=matches,
        tolerance=COLLATERAL_TOLERANCE,
        mismatches=mismatches,
        legs=[o.journal() for o in linked],
        before=_collateral_rows(before),
        after=_collateral_rows(after),
    )


def _far_exchange(bridge: Bridge) -> str | None:
    """An exchange where a YES ask at 0.95 cannot fill (best bid well below)."""
    for ex, snap in sorted(bridge.market.prices.items()):
        if snap.best_bid is not None and snap.best_bid < 0.90:
            return ex
    return None


# ── the decision table ──────────────────────────────────────────────────────────────────────


def decide(report: Day1Report) -> list[str]:
    """SPEC §4, applied to what we could measure today."""
    recs: list[str] = []
    census_s = report.get("census")
    rate = report.get("rate_limit")
    lb = report.get("leaderboard")
    if census_s is not None and census_s.status == "ok":
        spread = census_s.data["spreadTicks"]["median"]
        depth = census_s.data["topDepth"]["median"]
        two_sided = census_s.data["twoSidedFrac"]
        if spread is not None and spread <= 4 and depth is not None and depth >= 100 and two_sided >= 0.8:
            recs.append(
                f"Row 1: the Sponsor quotes tight (median {spread:g} ticks = {spread * 0.005:.3f}) with size "
                f"(median top depth {depth:g}). Keep market making OFF; run the sniper only and move ~70% of "
                "engineering to election night."
            )
        elif two_sided < 0.5 or (spread is not None and spread > 8):
            recs.append(
                f"Row 2: the Sponsor is absent or wide (two-sided {two_sided:.0%}, median spread "
                f"{spread} ticks). Enable market making around fair (strategies.mm.enabled = true after an "
                "experiment PR)."
            )
        else:
            recs.append(
                f"Rows 1-2 inconclusive (median spread {spread} ticks, depth {depth}); keep sniper + racer, "
                "re-run the census tomorrow."
            )
        if census_s.data["lowPricedFrac"] >= 0.2:
            recs.append(
                f"Row 4: {census_s.data['lowPricedFrac']:.0%} of contracts trade at <= 5%. Raise the T "
                "estimates and boost election-night rolling capacity."
            )
    recs.append(
        "Row 3: no per-market position limit observed at probe size; re-check after the first real fills."
    )
    if rate is not None and rate.status == "ok":
        sustained = rate.data.get("sustainedRps") or 0
        limited = rate.data.get("firstLimitedAtRps")
        if limited is not None and limited < 5:
            recs.append(
                f"Row 7: rate limits are tight (first 429 at {limited} rps). Realtime first, batch reads, "
                "cancel sparingly, reserve capacity for election night; set platform.max_rps below the limit."
            )
        else:
            recs.append(
                f"Row 7: no 429 up to {sustained} rps (probe cap); keep the adaptive limiter defaults."
            )
    recs.append("Row 5 (D drift vs Kalshi) needs a week of price snapshots: revisit Oct 7.")
    recs.append(_collateral_recommendation(report.get("collateral")))
    if lb is not None and lb.status == "ok" and lb.data.get("topPnl") is not None:
        recs.append(
            f"Row 6: top leaderboard P&L today is {lb.data['topPnl']:+.0f} SUSQies; watch for >= 3x "
            "balances by mid-October (mark manipulation or illiquid marks)."
        )
    return recs


def _collateral_recommendation(section: Section | None) -> str:
    if section is None or section.status in ("skipped", "pending"):
        why = section.summary if section is not None else "not probed"
        return f"Collateral: not measured yet ({why}); keep [risk] use_collateral_buying_power = false."
    if section.status == "ok" and section.data.get("matches"):
        return (
            f"Collateral: the engine's `all` economics matched the model within "
            f"{section.data.get('tolerance')} SUSQies on {section.data.get('filled')} fill(s) "
            f"(savings {section.data.get('savings')}): set use_collateral_buying_power = true in "
            "config/susq.toml (experiment PR)."
        )
    return f"Collateral: keep [risk] use_collateral_buying_power = false. {section.summary}"


# ── orchestration ───────────────────────────────────────────────────────────────────────────


async def measure_day1(
    config: BridgeConfig,
    *,
    bridge: Bridge | None,
    transport: httpx.AsyncBaseTransport | None = None,
    clock: Callable[[], datetime] = lambda: datetime.now(UTC),
    rate_probe: bool = True,
    rate_steps: tuple[float, ...] = (2, 4, 6, 8, 10),
    rate_step_s: float = 2.0,
    realtime_probe: bool = True,
) -> Day1Report:
    """Run every probe that the current mode and credentials allow; never raises for a failed probe."""
    report = Day1Report(generated_at=clock().isoformat(), mode=bridge.mode if bridge else "none")
    if not config.has_api_key or bridge is None:
        for name in (
            "hosts",
            "tournament",
            "tick",
            "no_mechanics",
            "rate_limit",
            "realtime",
            "census",
            "collateral",
        ):
            report.add(name, "skipped", "no API key")
        report.action_needed.append("Create an API key (read + trade) and put SUSQ_API_KEY in .env")
        report.recommendations = decide(report)
        return report

    async def section(name: str, coro: Any) -> Any:
        try:
            return await coro
        except Exception as exc:
            report.add(name, "failed", f"{type(exc).__name__}: {exc}")
            return None

    if (hosts := await section("hosts", probe_hosts(config, transport))) is not None:
        working = [k for k, v in hosts.items() if v.get("ok")]
        report.add(
            "hosts", "ok" if working else "failed", f"working: {', '.join(working) or 'none'}", **hosts
        )
        if hosts.get("primary", {}).get("ok") is False and "alternate" in working:
            report.action_needed.append(
                f"Primary host failed; set SUSQ_BASE_URL={config.platform.alt_base_url}"
            )

    t = bridge.tournament
    report.add(
        "tournament",
        "ok",
        f"{t.slug} ({t.id}): {t.member_count} members, {t.market_count} markets, balance {t.my_balance}",
        id=t.id,
        slug=t.slug,
        memberCount=t.member_count,
        marketCount=t.market_count,
        myBalance=t.my_balance,
        pendingEnrolment=t.is_pending_enrolment,
    )

    if bridge.mode == "live":
        await section("tick", probe_writes(bridge, report))
    else:
        report.add("tick", "skipped", "paper mode: the off-tick probe is live-only")
        report.add("no_mechanics", "skipped", "paper mode: the NO canonicalization probe is live-only")
    report.add(
        "market_vs_limit",
        "skipped",
        "not probed by design: Susquatch never sends market orders "
        "(documented: omitted price, 1 on a buy, 0 on a sell)",
    )

    if rate_probe:
        probe = probe_rate_limit(bridge.client, steps=rate_steps, seconds_per_step=rate_step_s)
        if (rate := await section("rate_limit", probe)) is not None:
            report.add(
                "rate_limit",
                "ok",
                f"first 429 at {rate['firstLimitedAtRps']} rps, Retry-After {rate['retryAfterS']}; "
                f"sustained {rate['sustainedRps']} rps",
                **rate,
            )
    else:
        report.add("rate_limit", "skipped", "disabled")

    if realtime_probe:
        if (rt := await section("realtime", probe_realtime(bridge))) is not None:
            report.add("realtime", "ok" if rt["joined"] else "failed", f"joined: {rt['joined']}", **rt)
    else:
        report.add("realtime", "skipped", "disabled")

    if (c := await section("census", census(bridge))) is not None:
        report.add(
            "census",
            "ok",
            f"{c['exchanges']} exchanges, {c['twoSidedFrac']:.0%} two-sided, median spread "
            f"{c['spreadTicks']['median']} ticks, median top depth {c['topDepth']['median']}",
            **c,
        )

    slug = config.platform.tournament_slug
    lb_coro = bridge.gateway.read("leaderboard", lambda: bridge.client.get_leaderboard(slug, limit=10))
    if (lb := await section("leaderboard", lb_coro)) is not None:
        top = lb.leaderboard[0].pnl if lb.leaderboard else None
        report.add(
            "leaderboard",
            "ok",
            f"{lb.total} ranked, myRank {lb.my_rank}",
            total=lb.total,
            myRank=lb.my_rank,
            topPnl=top,
        )

    if (cp := await section("constraint_prices", probe_constraint_prices(bridge))) is not None:
        report.add(
            "constraint_prices",
            "ok",
            f"{cp['observedCompared']} observed prices: {cp['matchLastTrade']} equal the last trade, "
            f"{cp['matchBookMid']} within half a tick of the book mid",
            **cp,
        )

    await section("collateral", probe_collateral(bridge, report))

    report.add("fees", "pending", "needs the first fill (first_trade records totalCost vs quantity x price)")
    report.add(
        "valuation", "pending", "needs the first fill (first_trade compares currentPrice to trade and mid)"
    )
    report.recommendations = decide(report)
    return report


# ── persistence ─────────────────────────────────────────────────────────────────────────────


def write_report(report: Day1Report, reports_dir: Path) -> tuple[Path, Path]:
    reports_dir.mkdir(parents=True, exist_ok=True)
    json_path = reports_dir / "day1.json"
    md_path = reports_dir / "DAY1.md"
    json_path.write_bytes(orjson.dumps(asdict(report), option=orjson.OPT_INDENT_2))
    lines = [
        "# DAY 1 — platform measurements",
        "",
        f"Generated {report.generated_at} in **{report.mode}** mode. "
        "Competition Data: never commit this file.",
        "",
    ]
    if report.action_needed:
        lines += ["## ACTION NEEDED", "", *(f"- {a}" for a in report.action_needed), ""]
    lines += ["## Measurements", "", "| Probe | Status | Result |", "|---|---|---|"]
    lines += [f"| {s.name} | {s.status} | {s.summary.replace('|', '/')} |" for s in report.sections]
    lines += ["", "## Decision table (SPEC §4)", "", *(f"- {r}" for r in report.recommendations), ""]
    md_path.write_text("\n".join(lines))
    return md_path, json_path


def load_report(reports_dir: Path) -> Day1Report | None:
    path = reports_dir / "day1.json"
    if not path.exists():
        return None
    raw = orjson.loads(path.read_bytes())
    report = Day1Report(
        generated_at=raw["generated_at"],
        mode=raw["mode"],
        recommendations=raw.get("recommendations", []),
        action_needed=raw.get("action_needed", []),
    )
    report.sections = [Section(**s) for s in raw.get("sections", [])]
    return report


def record_first_fill(reports_dir: Path, fill: dict[str, Any]) -> Day1Report:
    """Fill in the fee and valuation sections once the first trade has executed."""
    report = load_report(reports_dir) or Day1Report(generated_at=datetime.now(UTC).isoformat(), mode="live")
    qty, price, total = fill["quantity"], fill["price"], fill["totalCost"]
    fee = round(total - qty * price, 6)
    report.add(
        "fees",
        "ok",
        f"totalCost {total} vs quantity x price {qty * price:.4f}: implied fee {fee}",
        quantity=qty,
        price=price,
        totalCost=total,
        impliedFee=fee,
    )
    current, mid = fill.get("currentPrice"), fill.get("midAfter")
    basis = (
        "last trade"
        if current == price
        else ("book mid" if mid is not None and current == mid else "unknown")
    )
    report.add(
        "valuation",
        "ok",
        f"position currentPrice {current} vs our fill {price} and mid {mid}: valued at {basis}",
        currentPrice=current,
        fillPrice=price,
        midAfter=mid,
        basis=basis,
    )
    report.recommendations = decide(report)
    write_report(report, reports_dir)
    return report
