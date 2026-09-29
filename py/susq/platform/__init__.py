"""The Super Market platform layer: prices, errors, models, REST client, realtime, paper broker.

Everything that talks to (or pretends to be) the exchange for the Python side of Susquatch lives
here. The bridge's gateway wraps it with rate limiting, retries, journaling and risk.
"""

from susq.platform.client import HttpRecord, SuperMarketClient, TradingApi
from susq.platform.errors import ApiError, NetworkError, PlatformError, RetryClass
from susq.platform.models import OrderRequest
from susq.platform.prices import MAX_PIPS, MIN_PIPS, TICK_PIPS, Pips, to_pips, to_price

__all__ = [
    "MAX_PIPS",
    "MIN_PIPS",
    "TICK_PIPS",
    "ApiError",
    "HttpRecord",
    "NetworkError",
    "OrderRequest",
    "Pips",
    "PlatformError",
    "RetryClass",
    "SuperMarketClient",
    "TradingApi",
    "to_pips",
    "to_price",
]
