"""EXP-015: a paper run starts from the live account's cash as risk measures it."""

from __future__ import annotations

from typing import Any

from susq.platform.models import PnL, Tournament
from susq_bridge.runtime import _paper_seed


class _Limiter:
    async def acquire(self, kind: str) -> None:
        return None


class _Client:
    def __init__(self, pnl: PnL | Exception) -> None:
        self.pnl = pnl

    async def get_pnl(self, slug: str, *, period: str = "all") -> PnL:
        if isinstance(self.pnl, Exception):
            raise self.pnl
        return self.pnl


def _tournament(**kw: Any) -> Tournament:
    return Tournament.model_validate({"id": "t", "slug": "midterm-elections", **kw})


async def _seed(pnl: PnL | Exception, **kw: Any) -> float:
    return await _paper_seed(_Client(pnl), _Limiter(), _tournament(**kw))  # type: ignore[arg-type]


async def test_seed_is_account_value_minus_holdings() -> None:
    pnl = PnL(total_account_value=98_442.18, total_holdings_value=81_137.19)
    assert abs(await _seed(pnl, myBalance=27_254.41) - 17_304.99) < 1e-6


async def test_seed_falls_back_to_my_balance() -> None:
    assert await _seed(RuntimeError("down"), myBalance=27_254.41) == 27_254.41
    assert await _seed(PnL(total_account_value=98_442.18), myBalance=27_254.41) == 27_254.41
    assert await _seed(RuntimeError("down"), initialBalance=100_000.0) == 100_000.0


async def test_paper_bridge_starts_from_the_live_cash(harness, paper_bridge) -> None:  # type: ignore[no-untyped-def]
    pnl = await paper_bridge.client.get_pnl(paper_bridge.tournament.slug, period="all")
    assert pnl.total_holdings_value is not None
    expected = pnl.total_account_value - pnl.total_holdings_value
    assert paper_bridge.gateway.paper is not None
    assert abs(paper_bridge.gateway.paper.ledger.cash - expected) < 1e-6
