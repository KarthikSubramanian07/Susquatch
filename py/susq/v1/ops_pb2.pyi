from susq.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class IntentState(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    INTENT_STATE_UNSPECIFIED: _ClassVar[IntentState]
    INTENT_STATE_PENDING: _ClassVar[IntentState]
    INTENT_STATE_SENT: _ClassVar[IntentState]
    INTENT_STATE_ACKED: _ClassVar[IntentState]
    INTENT_STATE_REJECTED: _ClassVar[IntentState]
    INTENT_STATE_UNKNOWN: _ClassVar[IntentState]
    INTENT_STATE_DONE: _ClassVar[IntentState]
INTENT_STATE_UNSPECIFIED: IntentState
INTENT_STATE_PENDING: IntentState
INTENT_STATE_SENT: IntentState
INTENT_STATE_ACKED: IntentState
INTENT_STATE_REJECTED: IntentState
INTENT_STATE_UNKNOWN: IntentState
INTENT_STATE_DONE: IntentState

class JournalEntry(_message.Message):
    __slots__ = ("ts_ns", "producer", "direction", "method", "path", "body_json", "status", "latency_us", "request_id", "idempotency_key", "correlation_id")
    TS_NS_FIELD_NUMBER: _ClassVar[int]
    PRODUCER_FIELD_NUMBER: _ClassVar[int]
    DIRECTION_FIELD_NUMBER: _ClassVar[int]
    METHOD_FIELD_NUMBER: _ClassVar[int]
    PATH_FIELD_NUMBER: _ClassVar[int]
    BODY_JSON_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    LATENCY_US_FIELD_NUMBER: _ClassVar[int]
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    IDEMPOTENCY_KEY_FIELD_NUMBER: _ClassVar[int]
    CORRELATION_ID_FIELD_NUMBER: _ClassVar[int]
    ts_ns: int
    producer: str
    direction: str
    method: str
    path: str
    body_json: str
    status: int
    latency_us: int
    request_id: str
    idempotency_key: str
    correlation_id: str
    def __init__(self, ts_ns: _Optional[int] = ..., producer: _Optional[str] = ..., direction: _Optional[str] = ..., method: _Optional[str] = ..., path: _Optional[str] = ..., body_json: _Optional[str] = ..., status: _Optional[int] = ..., latency_us: _Optional[int] = ..., request_id: _Optional[str] = ..., idempotency_key: _Optional[str] = ..., correlation_id: _Optional[str] = ...) -> None: ...

class OrderIntent(_message.Message):
    __slots__ = ("intent_id", "idempotency_key", "strategy", "exchange_id", "side", "action", "quantity", "price_pips", "expiration_ns", "tournament_id", "state", "created_at_ns", "updated_at_ns", "order_id", "last_error_code", "attempts")
    INTENT_ID_FIELD_NUMBER: _ClassVar[int]
    IDEMPOTENCY_KEY_FIELD_NUMBER: _ClassVar[int]
    STRATEGY_FIELD_NUMBER: _ClassVar[int]
    EXCHANGE_ID_FIELD_NUMBER: _ClassVar[int]
    SIDE_FIELD_NUMBER: _ClassVar[int]
    ACTION_FIELD_NUMBER: _ClassVar[int]
    QUANTITY_FIELD_NUMBER: _ClassVar[int]
    PRICE_PIPS_FIELD_NUMBER: _ClassVar[int]
    EXPIRATION_NS_FIELD_NUMBER: _ClassVar[int]
    TOURNAMENT_ID_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    CREATED_AT_NS_FIELD_NUMBER: _ClassVar[int]
    UPDATED_AT_NS_FIELD_NUMBER: _ClassVar[int]
    ORDER_ID_FIELD_NUMBER: _ClassVar[int]
    LAST_ERROR_CODE_FIELD_NUMBER: _ClassVar[int]
    ATTEMPTS_FIELD_NUMBER: _ClassVar[int]
    intent_id: str
    idempotency_key: str
    strategy: str
    exchange_id: str
    side: _common_pb2.Side
    action: _common_pb2.Action
    quantity: int
    price_pips: int
    expiration_ns: int
    tournament_id: str
    state: IntentState
    created_at_ns: int
    updated_at_ns: int
    order_id: int
    last_error_code: str
    attempts: int
    def __init__(self, intent_id: _Optional[str] = ..., idempotency_key: _Optional[str] = ..., strategy: _Optional[str] = ..., exchange_id: _Optional[str] = ..., side: _Optional[_Union[_common_pb2.Side, str]] = ..., action: _Optional[_Union[_common_pb2.Action, str]] = ..., quantity: _Optional[int] = ..., price_pips: _Optional[int] = ..., expiration_ns: _Optional[int] = ..., tournament_id: _Optional[str] = ..., state: _Optional[_Union[IntentState, str]] = ..., created_at_ns: _Optional[int] = ..., updated_at_ns: _Optional[int] = ..., order_id: _Optional[int] = ..., last_error_code: _Optional[str] = ..., attempts: _Optional[int] = ...) -> None: ...

class TraderLease(_message.Message):
    __slots__ = ("holder", "host", "pid", "acquired_at_ns", "expires_at_ns", "epoch")
    HOLDER_FIELD_NUMBER: _ClassVar[int]
    HOST_FIELD_NUMBER: _ClassVar[int]
    PID_FIELD_NUMBER: _ClassVar[int]
    ACQUIRED_AT_NS_FIELD_NUMBER: _ClassVar[int]
    EXPIRES_AT_NS_FIELD_NUMBER: _ClassVar[int]
    EPOCH_FIELD_NUMBER: _ClassVar[int]
    holder: str
    host: str
    pid: int
    acquired_at_ns: int
    expires_at_ns: int
    epoch: int
    def __init__(self, holder: _Optional[str] = ..., host: _Optional[str] = ..., pid: _Optional[int] = ..., acquired_at_ns: _Optional[int] = ..., expires_at_ns: _Optional[int] = ..., epoch: _Optional[int] = ...) -> None: ...

class HealthItem(_message.Message):
    __slots__ = ("name", "state", "detail", "last_ok_ns")
    NAME_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    DETAIL_FIELD_NUMBER: _ClassVar[int]
    LAST_OK_NS_FIELD_NUMBER: _ClassVar[int]
    name: str
    state: str
    detail: str
    last_ok_ns: int
    def __init__(self, name: _Optional[str] = ..., state: _Optional[str] = ..., detail: _Optional[str] = ..., last_ok_ns: _Optional[int] = ...) -> None: ...

class PositionSummary(_message.Message):
    __slots__ = ("exchange_id", "title", "quantity", "avg_cost", "mark", "unrealized")
    EXCHANGE_ID_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    QUANTITY_FIELD_NUMBER: _ClassVar[int]
    AVG_COST_FIELD_NUMBER: _ClassVar[int]
    MARK_FIELD_NUMBER: _ClassVar[int]
    UNREALIZED_FIELD_NUMBER: _ClassVar[int]
    exchange_id: str
    title: str
    quantity: int
    avg_cost: float
    mark: float
    unrealized: float
    def __init__(self, exchange_id: _Optional[str] = ..., title: _Optional[str] = ..., quantity: _Optional[int] = ..., avg_cost: _Optional[float] = ..., mark: _Optional[float] = ..., unrealized: _Optional[float] = ...) -> None: ...

class TradeSummary(_message.Message):
    __slots__ = ("ts_ns", "strategy", "title", "side", "action", "quantity", "price")
    TS_NS_FIELD_NUMBER: _ClassVar[int]
    STRATEGY_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    SIDE_FIELD_NUMBER: _ClassVar[int]
    ACTION_FIELD_NUMBER: _ClassVar[int]
    QUANTITY_FIELD_NUMBER: _ClassVar[int]
    PRICE_FIELD_NUMBER: _ClassVar[int]
    ts_ns: int
    strategy: str
    title: str
    side: _common_pb2.Side
    action: _common_pb2.Action
    quantity: int
    price: float
    def __init__(self, ts_ns: _Optional[int] = ..., strategy: _Optional[str] = ..., title: _Optional[str] = ..., side: _Optional[_Union[_common_pb2.Side, str]] = ..., action: _Optional[_Union[_common_pb2.Action, str]] = ..., quantity: _Optional[int] = ..., price: _Optional[float] = ...) -> None: ...

class StatusSnapshot(_message.Message):
    __slots__ = ("header", "mode", "stage", "trader", "equity", "cash", "pnl_day", "pnl_total", "rank", "field_size", "killed", "kill_reason", "health", "positions", "recent_trades", "action_needed", "events")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    MODE_FIELD_NUMBER: _ClassVar[int]
    STAGE_FIELD_NUMBER: _ClassVar[int]
    TRADER_FIELD_NUMBER: _ClassVar[int]
    EQUITY_FIELD_NUMBER: _ClassVar[int]
    CASH_FIELD_NUMBER: _ClassVar[int]
    PNL_DAY_FIELD_NUMBER: _ClassVar[int]
    PNL_TOTAL_FIELD_NUMBER: _ClassVar[int]
    RANK_FIELD_NUMBER: _ClassVar[int]
    FIELD_SIZE_FIELD_NUMBER: _ClassVar[int]
    KILLED_FIELD_NUMBER: _ClassVar[int]
    KILL_REASON_FIELD_NUMBER: _ClassVar[int]
    HEALTH_FIELD_NUMBER: _ClassVar[int]
    POSITIONS_FIELD_NUMBER: _ClassVar[int]
    RECENT_TRADES_FIELD_NUMBER: _ClassVar[int]
    ACTION_NEEDED_FIELD_NUMBER: _ClassVar[int]
    EVENTS_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    mode: str
    stage: str
    trader: str
    equity: float
    cash: float
    pnl_day: float
    pnl_total: float
    rank: int
    field_size: int
    killed: bool
    kill_reason: str
    health: _containers.RepeatedCompositeFieldContainer[HealthItem]
    positions: _containers.RepeatedCompositeFieldContainer[PositionSummary]
    recent_trades: _containers.RepeatedCompositeFieldContainer[TradeSummary]
    action_needed: _containers.RepeatedScalarFieldContainer[str]
    events: _containers.RepeatedScalarFieldContainer[str]
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., mode: _Optional[str] = ..., stage: _Optional[str] = ..., trader: _Optional[str] = ..., equity: _Optional[float] = ..., cash: _Optional[float] = ..., pnl_day: _Optional[float] = ..., pnl_total: _Optional[float] = ..., rank: _Optional[int] = ..., field_size: _Optional[int] = ..., killed: _Optional[bool] = ..., kill_reason: _Optional[str] = ..., health: _Optional[_Iterable[_Union[HealthItem, _Mapping]]] = ..., positions: _Optional[_Iterable[_Union[PositionSummary, _Mapping]]] = ..., recent_trades: _Optional[_Iterable[_Union[TradeSummary, _Mapping]]] = ..., action_needed: _Optional[_Iterable[str]] = ..., events: _Optional[_Iterable[str]] = ...) -> None: ...

class MarketMap(_message.Message):
    __slots__ = ("header", "markets")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    MARKETS_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    markets: _containers.RepeatedCompositeFieldContainer[MarketRule]
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., markets: _Optional[_Iterable[_Union[MarketRule, _Mapping]]] = ...) -> None: ...

class MarketRule(_message.Message):
    __slots__ = ("market_id", "exchange_id", "title", "race_id", "kind", "party", "resolution_source", "independent_can_win", "rcv", "runoff_possible", "manual_settlement", "kalshi_ticker", "kalshi_inverted", "polymarket_token_id", "polymarket_inverted", "clusters", "verified", "notes")
    MARKET_ID_FIELD_NUMBER: _ClassVar[int]
    EXCHANGE_ID_FIELD_NUMBER: _ClassVar[int]
    TITLE_FIELD_NUMBER: _ClassVar[int]
    RACE_ID_FIELD_NUMBER: _ClassVar[int]
    KIND_FIELD_NUMBER: _ClassVar[int]
    PARTY_FIELD_NUMBER: _ClassVar[int]
    RESOLUTION_SOURCE_FIELD_NUMBER: _ClassVar[int]
    INDEPENDENT_CAN_WIN_FIELD_NUMBER: _ClassVar[int]
    RCV_FIELD_NUMBER: _ClassVar[int]
    RUNOFF_POSSIBLE_FIELD_NUMBER: _ClassVar[int]
    MANUAL_SETTLEMENT_FIELD_NUMBER: _ClassVar[int]
    KALSHI_TICKER_FIELD_NUMBER: _ClassVar[int]
    KALSHI_INVERTED_FIELD_NUMBER: _ClassVar[int]
    POLYMARKET_TOKEN_ID_FIELD_NUMBER: _ClassVar[int]
    POLYMARKET_INVERTED_FIELD_NUMBER: _ClassVar[int]
    CLUSTERS_FIELD_NUMBER: _ClassVar[int]
    VERIFIED_FIELD_NUMBER: _ClassVar[int]
    NOTES_FIELD_NUMBER: _ClassVar[int]
    market_id: str
    exchange_id: str
    title: str
    race_id: str
    kind: str
    party: str
    resolution_source: str
    independent_can_win: bool
    rcv: bool
    runoff_possible: bool
    manual_settlement: bool
    kalshi_ticker: str
    kalshi_inverted: bool
    polymarket_token_id: str
    polymarket_inverted: bool
    clusters: _containers.RepeatedScalarFieldContainer[str]
    verified: bool
    notes: str
    def __init__(self, market_id: _Optional[str] = ..., exchange_id: _Optional[str] = ..., title: _Optional[str] = ..., race_id: _Optional[str] = ..., kind: _Optional[str] = ..., party: _Optional[str] = ..., resolution_source: _Optional[str] = ..., independent_can_win: _Optional[bool] = ..., rcv: _Optional[bool] = ..., runoff_possible: _Optional[bool] = ..., manual_settlement: _Optional[bool] = ..., kalshi_ticker: _Optional[str] = ..., kalshi_inverted: _Optional[bool] = ..., polymarket_token_id: _Optional[str] = ..., polymarket_inverted: _Optional[bool] = ..., clusters: _Optional[_Iterable[str]] = ..., verified: _Optional[bool] = ..., notes: _Optional[str] = ...) -> None: ...
