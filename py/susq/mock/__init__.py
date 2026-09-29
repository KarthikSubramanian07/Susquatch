"""A faithful, deterministic mock of The Super Market for tests and paper rehearsals.

See ``py/susq/mock/README.md`` for the endpoints, semantics, fault injection and admin API.
"""

from susq.mock.server import MockConfig, MockExchange, create_app

__all__ = ["MockConfig", "MockExchange", "create_app"]
