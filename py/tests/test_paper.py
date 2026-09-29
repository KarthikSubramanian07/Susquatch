"""PaperBroker: fills against real books, virtual resting orders, and platform-identical failures."""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

import pytest
from susq.mock.testing import MockEnv
from susq.platform.collateral import Relation
from susq.platform.errors import ApiError
from susq.platform.models import BookLevel, OrderBook, OrderRequest
from susq.platform.paper import PaperBroker
from susq.platform.prices import to_pips

T = datetime(2026, 10, 1, 16, 0, tzinfo=UTC)
EX = "77"


class Books:
    def __init__(self) -> None:
        self.book = OrderBook(exchange_id=EX, bids=[], asks=[])
        self.calls = 0

    def set(self, bids: list[tuple[float, float]] = (), asks: list[tuple[float, float]] = ()) -> OrderBook:  # type: ignore[assignment]
        self.book = OrderBook(
            exchange_id=EX,
            bids=[BookLevel(price=p, quantity=q) for p, q in bids],
            asks=[BookLevel(price=p, quantity=q) for p, q in asks],
        )
        return self.book

    async def get_orderbook(self, exchange_id: str, tournament_id: str, *, depth: int = 20) -> OrderBook:
        self.calls += 1
        return self.book


class Clock:
    def __init__(self) -> None:
        self.now = T

    def __call__(self) -> datetime:
        return self.now


@pytest.fixture
def books() -> Books:
    return Books()


@pytest.fixture
def clock() -> Clock:
    return Clock()


@pytest.fixture
def paper(books: Books, clock: Clock) -> PaperBroker:
    return PaperBroker(books, "t", starting_cash=1_000, clock=clock)


def req(
    side: str = "yes",
    action: str = "buy",
    qty: int = 10,
    price: float = 0.60,
    ttl: int = 300,
    ex: str = EX,
) -> OrderRequest:
    return OrderRequest(ex, side, action, qty, to_pips(price), T + timedelta(seconds=ttl), "t")  # type: ignore[arg-type]


async def test_marketable_part_walks_depth_and_remainder_rests(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.55, 5), (0.60, 5), (0.65, 5)])
    res = await paper.place_order(req(qty=12, price=0.60), "k1")
    assert res.quantity_traded == 10
    assert res.total_cost == pytest.approx(5 * 0.55 + 5 * 0.60)
    assert res.open
    assert res.remaining_quantity == 2
    assert paper.ledger.position(EX) == 10
    assert [o.quantity for o in await paper.list_open_orders("t")] == [2]


async def test_taken_liquidity_is_not_reused_until_the_level_changes(
    paper: PaperBroker, books: Books
) -> None:
    books.set(asks=[(0.55, 5)])
    await paper.place_order(req(qty=5, price=0.55), "k1")
    second = await paper.place_order(req(qty=5, price=0.55), "k2")
    assert second.quantity_traded == 0
    books.set(asks=[(0.55, 7)])  # the real level changed: fresh liquidity
    third = await paper.place_order(req(qty=3, price=0.55), "k3")
    assert third.quantity_traded == 3


async def test_resting_order_fills_only_when_book_trades_strictly_through(
    paper: PaperBroker, books: Books
) -> None:
    books.set(asks=[(0.65, 10)])
    res = await paper.place_order(req(qty=4, price=0.60), "k1")
    assert res.open
    assert paper.on_book(books.set(asks=[(0.60, 10)])) == 0  # equal price: we'd be queued behind
    assert paper.on_book(books.set(asks=[(0.58, 3), (0.59, 10)])) == 4
    assert paper.ledger.position(EX) == 4
    assert paper.ledger.cash == pytest.approx(1_000 - 4 * 0.60)  # resting orders fill at their limit
    assert await paper.list_open_orders("t") == []


async def test_poll_refreshes_books(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.70, 10)])
    await paper.place_order(req(qty=2, price=0.60), "k1")
    books.set(asks=[(0.50, 10)])
    assert await paper.poll() == 2


async def test_expired_orders_never_fill(paper: PaperBroker, books: Books, clock: Clock) -> None:
    books.set(asks=[(0.70, 10)])
    await paper.place_order(req(qty=2, price=0.60, ttl=10), "k1")
    clock.now = T + timedelta(seconds=11)
    assert paper.on_book(books.set(asks=[(0.50, 10)])) == 0
    assert await paper.list_open_orders("t") == []
    assert paper.available_cash == pytest.approx(1_000)


async def test_expired_request_is_rejected(paper: PaperBroker, clock: Clock) -> None:
    clock.now = T + timedelta(hours=1)
    with pytest.raises(ApiError):
        await paper.place_order(req(), "k")


async def test_flat_sell_becomes_complement_buy(paper: PaperBroker, books: Books) -> None:
    books.set(bids=[(0.45, 10)])
    res = await paper.place_order(req(side="yes", action="sell", qty=3, price=0.40), "k1")
    assert (res.action, res.side, res.price) == ("buy", "no", 0.6)
    assert res.quantity_traded == 3
    assert paper.ledger.position(EX) == -3
    assert paper.ledger.cash == pytest.approx(1_000 - 3 * 0.55)


async def test_backed_sell_stays_a_sell(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.50, 10)])
    await paper.place_order(req(qty=5, price=0.50), "k1")
    books.set(bids=[(0.55, 10)])
    res = await paper.place_order(req(action="sell", qty=5, price=0.55), "k2")
    assert (res.action, res.side) == ("sell", "yes")
    assert paper.ledger.position(EX) == 0
    assert paper.ledger.cash == pytest.approx(1_000 + 5 * 0.05)


async def test_self_trade_prevention(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.70, 10)])
    await paper.place_order(req(side="no", qty=5, price=0.40), "k-ask")  # our ask at YES 0.60
    books.set(asks=[(0.58, 3)])
    res = await paper.place_order(req(qty=10, price=0.65), "k-bid")
    assert res.quantity_traded == 3
    assert res.terminal_reason_code == "SelfTradePrevented"
    assert not res.open


async def test_insufficient_balance(paper: PaperBroker) -> None:
    with pytest.raises(ApiError) as info:
        await paper.place_order(req(qty=10_000, price=0.50), "k")
    assert info.value.details["reason"] == "insufficient_balance"


async def test_idempotency(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.70, 10)])
    a = await paper.place_order(req(), "k")
    b = await paper.place_order(req(), "k")
    assert a.order_id == b.order_id
    assert books.calls == 1
    with pytest.raises(ApiError) as info:
        await paper.place_order(req(qty=11), "k")
    assert info.value.code == "CONFLICT"


async def test_multi_leg_rolls_back(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.50, 100)])
    legs = [req(qty=10, price=0.50), req(qty=5_000, price=0.50, ex="78")]
    with pytest.raises(ApiError) as info:
        await paper.place_multi_leg(legs, "ml")
    assert info.value.details["leg"] == 1
    assert paper.ledger.position(EX) == 0
    assert paper.ledger.cash == pytest.approx(1_000)
    assert await paper.list_open_orders("t") == []


async def test_multi_leg_rejects_relationship_constraint_and_duplicates(paper: PaperBroker) -> None:
    with pytest.raises(ApiError):
        await paper.place_multi_leg([req()], "ml", relationship_constraint="r")
    with pytest.raises(ApiError) as info:
        await paper.place_multi_leg([req(), req(qty=2)], "ml2")
    assert info.value.status == 400


async def test_batch_statuses(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.90, 10)])
    ok = await paper.place_batch([req(qty=1, price=0.10)], "b1")
    assert ok.http_status == 200
    mixed = await paper.place_batch([req(qty=1, price=0.10), req(qty=100_000, price=0.10)], "b2")
    assert mixed.http_status == 207
    failed = await paper.place_batch([req(qty=100_000, price=0.10)], "b3")
    assert failed.http_status == 422


async def test_cancels(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.90, 10)])
    a = await paper.place_order(req(qty=1, price=0.10), "a")
    await paper.place_order(req(qty=1, price=0.15), "b")
    await paper.cancel_order(a.order_id or 0)
    with pytest.raises(ApiError) as info:
        await paper.cancel_order(a.order_id or 0)
    assert info.value.status == 409
    with pytest.raises(ApiError) as info:
        await paper.cancel_order(1)
    assert info.value.status == 404
    res = await paper.cancel_all("t")
    assert res.cancelled == 1
    assert paper.available_cash == pytest.approx(1_000)


async def test_positions_and_account_value(paper: PaperBroker, books: Books) -> None:
    books.set(asks=[(0.40, 10)])
    await paper.place_order(req(qty=10, price=0.40), "k")
    pos = paper.positions()
    assert pos.positions[0].quantity == 10
    assert pos.positions[0].current_price == pytest.approx(0.40)
    assert paper.account_value() == pytest.approx(1_000)


async def test_against_the_mock_exchange(env: MockEnv) -> None:
    """Integration: real client, mock books; a resting paper bid fills after the Sponsor re-marks."""
    state = await env.state()
    ex = "1309"  # Michigan D
    fair = to_pips(state["fair"][ex])
    paper = PaperBroker(env.client, env.tid, clock=env.mx.clock.now)
    book = await env.client.get_orderbook(ex, env.tid)
    ask = to_pips(book.asks[0].price)
    taken = await paper.place_order(env.order(ex, qty=3, price_pips=ask), "p1")
    assert taken.quantity_traded == 3
    resting = await paper.place_order(env.order(ex, qty=2, price_pips=(fair // 50) * 50 - 200), "p2")
    assert resting.open
    await env.post_admin("/_mock/fair", {"exchangeId": ex, "fair": (fair - 800) / 10_000})
    assert await paper.poll() == 2
    assert paper.ledger.position(ex) == 5
    # Nothing touched the real (mock) account: paper is invisible to the exchange.
    assert await env.client.list_open_orders(env.tid) == []
    assert (await env.client.get_positions("midterm-elections")).positions == []


# ── ALL collateral (same model as the mock) ─────────────────────────────────────────────────


class MultiBooks:
    """Per-exchange books."""

    def __init__(self, books: dict[str, OrderBook]) -> None:
        self.books = books

    async def get_orderbook(self, exchange_id: str, tournament_id: str, *, depth: int = 20) -> OrderBook:
        return self.books[exchange_id]


def _bid_book(ex: str, bid: float) -> OrderBook:
    return OrderBook(exchange_id=ex, bids=[BookLevel(price=bid, quantity=100)], asks=[])


PAIR = Relation("rel-1", "mutually_exclusive", ("81", "82"), is_exhaustive=True)


def _pair_books() -> MultiBooks:
    return MultiBooks({"81": _bid_book("81", 0.40), "82": _bid_book("82", 0.55)})


async def test_paper_no_bundle_gets_the_modeled_advance(clock: Clock) -> None:
    paper = PaperBroker(_pair_books(), "t", starting_cash=7, clock=clock, relationships=[PAIR])
    legs = [req("no", "buy", 10, 0.60, ex="81"), req("no", "buy", 10, 0.45, ex="82")]
    res = await paper.place_multi_leg(legs, "bundle")
    first, second = (item.data for item in res.results)
    assert first.all is not None
    assert second.all is not None
    assert first.all.collateral_savings == 0
    assert second.all.collateral_savings == pytest.approx(4.5)
    assert second.all.guaranteed_payout_floor_after == 10
    assert paper.ledger.cash == pytest.approx(1.0)  # 7 - 6.0; the 4.5 was advanced
    paper.marks.update({"81": 4_000, "82": 5_500})
    assert paper.account_value() == pytest.approx(1.0 + 10 * 0.60 + 10 * 0.45 - 4.5)
    (row,) = paper.get_collateral().data
    assert (row.component_id, row.outstanding_advance) == ("81-82", pytest.approx(4.5))


async def test_paper_without_relationships_or_with_collateral_off_is_plain_cash(clock: Clock) -> None:
    for paper in (
        PaperBroker(_pair_books(), "t", starting_cash=100, clock=clock),
        PaperBroker(
            _pair_books(), "t", starting_cash=100, clock=clock, relationships=[PAIR], collateral=False
        ),
    ):
        res = await paper.place_multi_leg(
            [req("no", "buy", 10, 0.60, ex="81"), req("no", "buy", 10, 0.45, ex="82")], "b"
        )
        assert all(item.data.all is None for item in res.results)
        assert paper.ledger.cash == pytest.approx(100 - 10.5)
        assert paper.get_collateral().data == []


async def test_paper_relationships_can_arrive_later(clock: Clock) -> None:
    paper = PaperBroker(_pair_books(), "t", starting_cash=100, clock=clock)
    paper.set_relationships([PAIR])
    await paper.place_order(req("no", "buy", 10, 0.60, ex="81"), "a")
    res = await paper.place_order(req("no", "buy", 10, 0.45, ex="82"), "b")
    assert res.all is not None
    assert res.all.effective_entry_cost == 0


async def test_paper_multi_leg_rollback_restores_advances(clock: Clock) -> None:
    books = _pair_books()
    books.books["83"] = OrderBook(exchange_id="83", bids=[], asks=[])
    paper = PaperBroker(books, "t", starting_cash=100, clock=clock, relationships=[PAIR])
    legs = [
        req("no", "buy", 10, 0.60, ex="81"),
        req("no", "buy", 10, 0.45, ex="82"),
        req("yes", "buy", 10_000, 0.50, ex="83"),  # cannot be afforded: the whole bundle rolls back
    ]
    with pytest.raises(ApiError):
        await paper.place_multi_leg(legs, "rollback")
    assert paper.collateral.outstanding == {}
    assert paper.ledger.cash == pytest.approx(100)
