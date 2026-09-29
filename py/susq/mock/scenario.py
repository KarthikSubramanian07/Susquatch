"""Deterministic synthetic Cup: markets, relationships, a Sponsor bot, stale student orders, a field.

Everything is derived from one integer seed, so two mocks started with the same seed serve
byte-identical markets, books, leaderboards and violations (as long as they receive the same
requests at the same mock-clock times).
"""

from __future__ import annotations

import random
import uuid
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

from susq.mock.engine import Account, Engine, MarketDef
from susq.platform.ledger import Ledger
from susq.platform.prices import (
    MAX_PIPS,
    MIN_PIPS,
    PIPS_PER_UNIT,
    TICK_PIPS,
    ceil_tick,
    clamp_limit,
    floor_tick,
    nearest_tick,
    to_price,
)

__all__ = ["PlayerDef", "RelationshipDef", "Scenario", "ScenarioConfig"]

_NS = uuid.UUID("0b7a3f51-3c1e-4f0e-9d6a-5a1f2e3d4c5b")
START_PIPS = 100_000 * PIPS_PER_UNIT

# (state, name, P(D wins), P(independent wins)). NE has no Democratic nominee of note and a strong
# independent, so its D/R pair is mutually exclusive but NOT exhaustive.
_SENATE_RACES: list[tuple[str, str, float, float]] = [
    ("OH", "Ohio", 0.58, 0.0),
    ("IA", "Iowa", 0.43, 0.0),
    ("ME", "Maine", 0.60, 0.0),
    ("TX", "Texas", 0.61, 0.0),
    ("MI", "Michigan", 0.72, 0.0),
    ("NH", "New Hampshire", 0.86, 0.0),
    ("GA", "Georgia", 0.94, 0.0),
    ("NE", "Nebraska", 0.03, 0.42),
]


def _uid(kind: str, key: str) -> str:
    return str(uuid.uuid5(_NS, f"{kind}:{key}"))


@dataclass(slots=True)
class RelationshipDef:
    id: str
    type: str  # mutually_exclusive | implication
    exchange_ids: list[str]  # implication: [antecedent, consequent]
    is_exhaustive: bool
    label: str


@dataclass(slots=True)
class PlayerDef:
    """A synthetic leaderboard entrant (never trades on the mock books)."""

    profile_id: str
    username: str
    value_pips: int
    trades_count: int
    volume: float
    win_rate: float | None


@dataclass
class ScenarioConfig:
    seed: int = 7
    tournament_slug: str = "midterm-elections"
    sponsor_half_width_pips: int = 100  # 0.01 each side of fair
    sponsor_size: int = 500
    stale_orders: int = 4
    stale_through_pips: int = 500  # student orders sit 5 points through fair
    synthetic_players: int = 40
    api_keys: dict[str, str] = field(default_factory=lambda: {"mock-key-alpha": "trader-alpha"})


class Scenario:
    """Owns the engine plus everything that isn't order matching."""

    def __init__(self, config: ScenarioConfig, now: datetime) -> None:
        self.config = config
        self.rng = random.Random(config.seed)  # noqa: S311 - simulation, not crypto
        self.tournament_id = _uid("tournament", config.tournament_slug)
        self.relationships: list[RelationshipDef] = []
        self.fair: dict[str, int] = {}
        self.sponsor_id = _uid("profile", "sponsor")
        self.students = [_uid("profile", f"student-{i}") for i in range(3)]
        self.keys: dict[str, str] = {}  # api key -> profile id
        self.players: list[PlayerDef] = []
        markets = self._build_markets()
        self.engine = Engine(self.tournament_id, markets)
        self._build_accounts()
        self._build_players()
        self._seed_books(now)

    # ── construction ────────────────────────────────────────────────────────────────────────

    def _build_markets(self) -> list[MarketDef]:
        markets: list[MarketDef] = []
        next_id = 301

        def add(title: str, fair: float, details: dict[str, Any], category: str = "election-outcome") -> str:
            nonlocal next_id
            m = MarketDef(
                market_id=str(next_id),
                exchange_id=str(next_id + 1000),
                title=title,
                category=category,
                contract_details=details,
            )
            markets.append(m)
            self.fair[m.exchange_id] = max(MIN_PIPS, min(MAX_PIPS, round(fair * PIPS_PER_UNIT)))
            next_id += 1
            return m.exchange_id

        for state, name, p_dem, p_ind in _SENATE_RACES:
            jitter = self.rng.uniform(-0.02, 0.02) if p_ind == 0 else 0.0
            p_d = min(0.97, max(0.03, p_dem + jitter))
            p_r = 1.0 - p_d - p_ind
            base = {"office": "Senate", "state": state, "cycle": 2026}
            dem = add(f"Will the Democratic Party win the {name} Senate race?", p_d, {**base, "party": "DEM"})
            rep = add(f"Will the Republican Party win the {name} Senate race?", p_r, {**base, "party": "REP"})
            exhaustive = p_ind == 0
            self.relationships.append(
                RelationshipDef(
                    id=_uid("relationship", f"senate-{state}"),
                    type="mutually_exclusive",
                    exchange_ids=[dem, rep],
                    is_exhaustive=exhaustive,
                    label=f"{name} Senate winner",
                )
            )
        add("Will Democrats win control of the House?", 0.92, {"office": "House", "kind": "chamber"})
        senate_ctl = add(
            "Will Democrats win control of the Senate?", 0.62, {"office": "Senate", "kind": "chamber"}
        )
        d52 = add(
            "Will Democrats hold 52 or more Senate seats?",
            0.35,
            {"office": "Senate", "kind": "seat_count", "threshold": 52},
        )
        self.relationships.append(
            RelationshipDef(
                id=_uid("relationship", "senate-52-implies-control"),
                type="implication",
                exchange_ids=[d52, senate_ctl],
                is_exhaustive=False,
                label="52+ D seats implies D Senate control",
            )
        )
        return markets

    def _build_accounts(self) -> None:
        eng = self.engine
        eng.add_account(Account(self.sponsor_id, "sponsor", Ledger(10**9 * PIPS_PER_UNIT), is_bot=True))
        for i, sid in enumerate(self.students):
            eng.add_account(Account(sid, f"student-{i}", Ledger(START_PIPS)))
        for key, name in self.config.api_keys.items():
            pid = _uid("profile", name)
            eng.add_account(Account(pid, name, Ledger(START_PIPS), api_key=key))
            self.keys[key] = pid

    def _build_players(self) -> None:
        for i in range(self.config.synthetic_players):
            # Every seventh player sits at exactly the starting balance: real ties for 1-1-3 ranks.
            drift = self.rng.lognormvariate(0.0, 0.08)
            value = START_PIPS if i % 7 == 3 else round(START_PIPS * drift)
            self.players.append(
                PlayerDef(
                    profile_id=_uid("profile", f"player-{i}"),
                    username=f"cup-player-{i:03d}",
                    value_pips=value,
                    trades_count=self.rng.randint(1, 250),
                    volume=round(self.rng.uniform(100, 50_000), 2),
                    win_rate=None if i % 5 == 0 else round(self.rng.uniform(20, 80), 1),
                )
            )

    def _seed_books(self, now: datetime) -> None:
        eng = self.engine
        a, b = self.students[0], self.students[1]
        # Opening prints so every exchange has a tournament valuation price.
        for rel in self.relationships:
            if rel.type == "mutually_exclusive" and rel.is_exhaustive:
                d, r = rel.exchange_ids
                p_d = clamp_limit(nearest_tick(self.fair[d]))
                eng.print_trade(a, b, d, p_d, 1, now)
                eng.print_trade(a, b, r, PIPS_PER_UNIT - p_d, 1, now)
        for ex, fair in self.fair.items():
            if eng.mark(ex) is None:
                eng.print_trade(a, b, ex, clamp_limit(nearest_tick(fair)), 1, now)
        # Stale student orders go in first; the Sponsor then quotes post-only around them.
        candidates = sorted(self.fair)
        for i, ex in enumerate(self.rng.sample(candidates, min(self.config.stale_orders, len(candidates)))):
            side = "bid" if i % 2 == 0 else "ask"
            self.inject_stale(ex, side, self.config.stale_through_pips, 40 + 10 * i, now)
        for ex in sorted(self.fair):
            self.requote(ex, now)

    # ── sponsor ─────────────────────────────────────────────────────────────────────────────

    def requote(self, exchange_id: str, now: datetime) -> None:
        """Cancel and re-post the Sponsor's two-sided quote (post-only: never crosses anyone)."""
        eng = self.engine
        for order in eng.open_orders(self.sponsor_id, now, exchange_id=exchange_id):
            eng.cancel(self.sponsor_id, order.id, now)
        fair = self.fair[exchange_id]
        hw = self.config.sponsor_half_width_pips
        bid = floor_tick(fair - hw)
        ask = ceil_tick(fair + hw)
        book = eng.book(exchange_id, now, depth=1)
        if book["asks"]:
            bid = min(bid, book["asks"][0][0] - TICK_PIPS)
        if book["bids"]:
            ask = max(ask, book["bids"][0][0] + TICK_PIPS)
        expiry = None
        size = self.config.sponsor_size
        if MIN_PIPS <= bid <= MAX_PIPS:
            eng.place(self.sponsor_id, self._body(exchange_id, "yes", "buy", bid, size, expiry), now)
        if MIN_PIPS <= ask <= MAX_PIPS:
            eng.place(
                self.sponsor_id,
                self._body(exchange_id, "no", "buy", PIPS_PER_UNIT - ask, size, expiry),
                now,
            )

    def _body(
        self, ex: str, side: str, action: str, price: int, qty: int, expiration: str | None
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "exchangeId": ex,
            "side": side,
            "action": action,
            "quantity": qty,
            "price": to_price(price),
            "tournamentId": self.tournament_id,
        }
        if expiration:
            body["expirationDate"] = expiration
        return body

    def set_sponsor(self, half_width_pips: int | None, size: int | None, now: datetime) -> None:
        if half_width_pips is not None:
            self.config.sponsor_half_width_pips = half_width_pips
        if size is not None:
            self.config.sponsor_size = size
        for ex in sorted(self.fair):
            self.requote(ex, now)

    def move_fair(self, exchange_id: str, fair_pips: int, now: datetime) -> None:
        """The Sponsor re-marks; resting student orders stay where they were (and go stale)."""
        self.fair[exchange_id] = max(MIN_PIPS, min(MAX_PIPS, fair_pips))
        self.requote(exchange_id, now)

    # ── students ────────────────────────────────────────────────────────────────────────────

    def inject_stale(self, exchange_id: str, side: str, through_pips: int, qty: int, now: datetime) -> int:
        """A student order ``through_pips`` through fair: a bid above fair or an ask below it.

        It models an order placed before the Sponsor re-marked: the Sponsor's quote is pulled, the
        student order rests, and the Sponsor re-quotes post-only around it.
        """
        eng = self.engine
        for order in eng.open_orders(self.sponsor_id, now, exchange_id=exchange_id):
            eng.cancel(self.sponsor_id, order.id, now)
        fair = self.fair[exchange_id]
        student = self.students[2]
        if side == "bid":
            price = clamp_limit(floor_tick(fair + through_pips))
            body = self._body(exchange_id, "yes", "buy", price, qty, None)
        else:
            price = clamp_limit(ceil_tick(fair - through_pips))
            body = self._body(exchange_id, "no", "buy", PIPS_PER_UNIT - price, qty, None)
        order_id = int(eng.place(student, body, now).body["orderId"])
        self.requote(exchange_id, now)
        return order_id

    # ── relationships & constraints ─────────────────────────────────────────────────────────

    def relationship(self, rel_id: str) -> RelationshipDef | None:
        return next((r for r in self.relationships if r.id == rel_id), None)

    def create_violation(self, rel_id: str | None, amount_pips: int, now: datetime) -> RelationshipDef:
        """Push fairs (and print trades) so ``rel_id`` is violated by ``amount_pips``."""
        rel = self.relationship(rel_id) if rel_id else None
        if rel is None:
            rel = next(r for r in self.relationships if r.type == "mutually_exclusive" and r.is_exhaustive)
        a, b = self.students[0], self.students[1]
        if rel.type == "mutually_exclusive":
            # Raise every leg; the rounding lands the printed sum at >= 1 + amount.
            per_leg = -(-amount_pips // len(rel.exchange_ids))
            for ex in rel.exchange_ids:
                mark = self.engine.mark(ex) or self.fair[ex]
                new = clamp_limit(ceil_tick(mark + per_leg))
                self.fair[ex] = new
                self.requote(ex, now)
                self.engine.print_trade(a, b, ex, new, 1, now)
        else:
            ante, cons = rel.exchange_ids
            cons_mark = self.engine.mark(cons) or self.fair[cons]
            new = clamp_limit(ceil_tick(cons_mark + amount_pips))
            self.fair[ante] = new
            self.requote(ante, now)
            self.engine.print_trade(a, b, ante, new, 1, now)
        return rel

    def evaluate(self, rel: RelationshipDef, min_violation: float = 0.0) -> dict[str, Any]:
        """Engine-style evaluation of one relationship at current tournament prices."""
        eng = self.engine
        marks = {ex: eng.mark(ex) for ex in rel.exchange_ids}
        observed = [
            {
                "exchangeId": ex,
                "price": to_price(p),
                "marketId": eng.by_exchange[ex].market_id,
                "marketTitle": eng.by_exchange[ex].title,
                "outcome": "YES",
                "currentPrice": to_price(p),
            }
            for ex, p in marks.items()
            if p is not None
        ]
        base: dict[str, Any] = {
            "relationshipId": rel.id,
            "type": rel.type,
            "tournamentId": self.tournament_id,
            "direction": self._direction(rel),
            "constraint": self._constraint(rel),
            "observedPrices": observed or None,
            "suggestedCorrectiveTrades": [],
            "diagnostic": None,
        }
        if any(p is None for p in marks.values()):
            return {
                **base,
                "violationAmount": 0.0,
                "reason": "One or more exchanges has no tournament price.",
                "bound": None,
                "lowerBound": None,
                "upperBound": None,
                "currentValue": None,
                "evaluationStatus": "unevaluable",
            }
        prices = [p for p in marks.values() if p is not None]
        trades: list[dict[str, Any]] = []
        if rel.type == "mutually_exclusive":
            total = sum(prices) / PIPS_PER_UNIT
            lower = 1.0 if rel.is_exhaustive else 0.0
            upper = 1.0
            if total > upper:
                amount = total - upper
                trades = [self._corrective(ex, "No", "Buy", "Sum of YES prices exceeds 1") for ex in marks]
            elif total < lower:
                amount = lower - total
                trades = [self._corrective(ex, "Yes", "Buy", "Exhaustive set sums below 1") for ex in marks]
            else:
                amount = 0.0
            bound, current = (upper if total >= lower else lower), total
        else:
            ante, cons = (marks[ex] or 0 for ex in rel.exchange_ids)
            lower, upper = 0.0, cons / PIPS_PER_UNIT
            current, bound = ante / PIPS_PER_UNIT, cons / PIPS_PER_UNIT
            amount = max(0.0, current - bound)
            if amount > 0:
                a_ex, c_ex = rel.exchange_ids
                trades = [
                    self._corrective(a_ex, "No", "Buy", "Antecedent priced above consequent"),
                    self._corrective(c_ex, "Yes", "Buy", "Consequent priced below antecedent"),
                ]
        amount = round(amount, 6)
        violated = amount > 0 and amount >= min_violation
        return {
            **base,
            "violationAmount": amount if violated else 0.0,
            "reason": "violated" if violated else "satisfied",
            "bound": bound,
            "lowerBound": lower,
            "upperBound": upper,
            "currentValue": round(current, 6),
            "evaluationStatus": "violated" if violated else "satisfied",
            "suggestedCorrectiveTrades": trades if violated else [],
        }

    def _corrective(self, ex: str, outcome_side: str, action: str, why: str) -> dict[str, Any]:
        m = self.engine.by_exchange[ex]
        mark = self.engine.mark(ex)
        return {
            "exchangeId": ex,
            "outcomeSide": outcome_side,
            "action": action,
            "rationale": why,
            "marketId": m.market_id,
            "marketTitle": m.title,
            "outcome": "YES",
            "currentPrice": to_price(mark) if mark is not None else None,
        }

    @staticmethod
    def _direction(rel: RelationshipDef) -> dict[str, Any]:
        if rel.type == "implication":
            return {
                "kind": "antecedent_to_consequent",
                "fromExchangeIds": [rel.exchange_ids[0]],
                "toExchangeIds": [rel.exchange_ids[1]],
                "description": "If the antecedent resolves YES, the consequent resolves YES.",
            }
        return {
            "kind": "symmetric",
            "fromExchangeIds": list(rel.exchange_ids),
            "toExchangeIds": list(rel.exchange_ids),
            "description": "At most one member resolves YES.",
        }

    @staticmethod
    def _constraint(rel: RelationshipDef) -> dict[str, Any]:
        if rel.type == "implication":
            return {
                "description": "The antecedent probability cannot exceed the consequent probability.",
                "priceRule": "P(antecedent) <= P(consequent)",
                "boundSemantics": "bound is the maximum allowed antecedent probability.",
                "evaluationEndpoint": f"/api/v1/relationships/constraints?relationshipId={rel.id}",
            }
        rule = "sum(P) = 1" if rel.is_exhaustive else "sum(P) <= 1"
        return {
            "description": "Mutually exclusive outcomes.",
            "priceRule": rule,
            "boundSemantics": "bound is the allowed sum of member probabilities.",
            "evaluationEndpoint": f"/api/v1/relationships/constraints?relationshipId={rel.id}",
        }

    def relationship_json(self, rel: RelationshipDef) -> dict[str, Any]:
        eng = self.engine
        if rel.type == "implication":
            roles = ["antecedent", "consequent"]
        else:
            roles = ["member"] * len(rel.exchange_ids)
        nodes = []
        for ex, role in zip(rel.exchange_ids, roles, strict=True):
            m = eng.by_exchange[ex]
            mark = eng.mark(ex)
            nodes.append(
                {
                    "exchangeId": ex,
                    "marketId": m.market_id,
                    "marketTitle": m.title,
                    "outcome": "YES",
                    "currentPrice": to_price(mark) if mark is not None else None,
                    "role": role,
                    "contractId": None,
                }
            )
        return {
            "id": rel.id,
            "type": rel.type,
            "source": "admin_tagged",
            "status": "active",
            "label": rel.label,
            "reason": None,
            "isExhaustive": rel.is_exhaustive,
            "expression": None,
            "nodes": nodes,
            "direction": self._direction(rel),
            "constraint": self._constraint(rel),
            "createdAt": None,
            "updatedAt": None,
            "version": 1,
        }

    # ── leaderboard ─────────────────────────────────────────────────────────────────────────

    def standings(self) -> list[dict[str, Any]]:
        """Ranked entries (competition ranking 1-1-3) for everyone who has traded."""
        rows: list[dict[str, Any]] = []
        for p in self.players:
            rows.append(
                {
                    "profileId": p.profile_id,
                    "username": p.username,
                    "value": p.value_pips,
                    "tradesCount": p.trades_count,
                    "volume": p.volume,
                    "winRate": p.win_rate,
                }
            )
        for acct in self.engine.state.accounts.values():
            if acct.is_bot or acct.trades_count == 0:
                continue
            rows.append(
                {
                    "profileId": acct.profile_id,
                    "username": acct.username,
                    "value": self.engine.account_value_pips(acct.profile_id),
                    "tradesCount": acct.trades_count,
                    "volume": round(acct.volume_pips / PIPS_PER_UNIT, 2),
                    "winRate": None,
                }
            )
        rows.sort(key=lambda r: (-r["value"], r["profileId"]))
        out: list[dict[str, Any]] = []
        prev_value: int | None = None
        rank = 0
        for i, row in enumerate(rows, start=1):
            if row["value"] != prev_value:
                rank = i
                prev_value = row["value"]
            pnl = (row["value"] - START_PIPS) / PIPS_PER_UNIT
            volume = row["volume"]
            roi = 0.0 if not volume else max(-500.0, min(500.0, round(pnl / volume * 100, 2)))
            out.append(
                {
                    "rank": rank,
                    "profileId": row["profileId"],
                    "username": row["username"],
                    "pnl": round(pnl, 4),
                    "tradesCount": row["tradesCount"],
                    "volume": volume,
                    "winRate": row["winRate"],
                    "roi": roi,
                }
            )
        return out

    def member_count(self) -> int:
        return len(self.players) + len(self.engine.state.accounts) - 1  # the Sponsor isn't a member
