from susq.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class TargetFile(_message.Message):
    __slots__ = ("header", "tournament_slug", "valid_until_ns", "inputs", "fair", "positions", "clusters", "tourney", "notes", "content_sha256")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    TOURNAMENT_SLUG_FIELD_NUMBER: _ClassVar[int]
    VALID_UNTIL_NS_FIELD_NUMBER: _ClassVar[int]
    INPUTS_FIELD_NUMBER: _ClassVar[int]
    FAIR_FIELD_NUMBER: _ClassVar[int]
    POSITIONS_FIELD_NUMBER: _ClassVar[int]
    CLUSTERS_FIELD_NUMBER: _ClassVar[int]
    TOURNEY_FIELD_NUMBER: _ClassVar[int]
    NOTES_FIELD_NUMBER: _ClassVar[int]
    CONTENT_SHA256_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    tournament_slug: str
    valid_until_ns: int
    inputs: _containers.RepeatedCompositeFieldContainer[InputHash]
    fair: _containers.RepeatedCompositeFieldContainer[FairValue]
    positions: _containers.RepeatedCompositeFieldContainer[TargetPosition]
    clusters: _containers.RepeatedCompositeFieldContainer[ClusterLimit]
    tourney: TourneyEstimate
    notes: str
    content_sha256: str
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., tournament_slug: _Optional[str] = ..., valid_until_ns: _Optional[int] = ..., inputs: _Optional[_Iterable[_Union[InputHash, _Mapping]]] = ..., fair: _Optional[_Iterable[_Union[FairValue, _Mapping]]] = ..., positions: _Optional[_Iterable[_Union[TargetPosition, _Mapping]]] = ..., clusters: _Optional[_Iterable[_Union[ClusterLimit, _Mapping]]] = ..., tourney: _Optional[_Union[TourneyEstimate, _Mapping]] = ..., notes: _Optional[str] = ..., content_sha256: _Optional[str] = ...) -> None: ...

class InputHash(_message.Message):
    __slots__ = ("name", "sha256", "as_of_ns")
    NAME_FIELD_NUMBER: _ClassVar[int]
    SHA256_FIELD_NUMBER: _ClassVar[int]
    AS_OF_NS_FIELD_NUMBER: _ClassVar[int]
    name: str
    sha256: str
    as_of_ns: int
    def __init__(self, name: _Optional[str] = ..., sha256: _Optional[str] = ..., as_of_ns: _Optional[int] = ...) -> None: ...

class FairValue(_message.Message):
    __slots__ = ("exchange_id", "market_id", "p_yes", "p_lo", "p_hi", "source")
    EXCHANGE_ID_FIELD_NUMBER: _ClassVar[int]
    MARKET_ID_FIELD_NUMBER: _ClassVar[int]
    P_YES_FIELD_NUMBER: _ClassVar[int]
    P_LO_FIELD_NUMBER: _ClassVar[int]
    P_HI_FIELD_NUMBER: _ClassVar[int]
    SOURCE_FIELD_NUMBER: _ClassVar[int]
    exchange_id: str
    market_id: str
    p_yes: float
    p_lo: float
    p_hi: float
    source: str
    def __init__(self, exchange_id: _Optional[str] = ..., market_id: _Optional[str] = ..., p_yes: _Optional[float] = ..., p_lo: _Optional[float] = ..., p_hi: _Optional[float] = ..., source: _Optional[str] = ...) -> None: ...

class TargetPosition(_message.Message):
    __slots__ = ("exchange_id", "market_id", "target_qty", "max_entry_price", "locked", "tranche", "cluster", "rationale")
    EXCHANGE_ID_FIELD_NUMBER: _ClassVar[int]
    MARKET_ID_FIELD_NUMBER: _ClassVar[int]
    TARGET_QTY_FIELD_NUMBER: _ClassVar[int]
    MAX_ENTRY_PRICE_FIELD_NUMBER: _ClassVar[int]
    LOCKED_FIELD_NUMBER: _ClassVar[int]
    TRANCHE_FIELD_NUMBER: _ClassVar[int]
    CLUSTER_FIELD_NUMBER: _ClassVar[int]
    RATIONALE_FIELD_NUMBER: _ClassVar[int]
    exchange_id: str
    market_id: str
    target_qty: int
    max_entry_price: float
    locked: bool
    tranche: str
    cluster: str
    rationale: str
    def __init__(self, exchange_id: _Optional[str] = ..., market_id: _Optional[str] = ..., target_qty: _Optional[int] = ..., max_entry_price: _Optional[float] = ..., locked: _Optional[bool] = ..., tranche: _Optional[str] = ..., cluster: _Optional[str] = ..., rationale: _Optional[str] = ...) -> None: ...

class ClusterLimit(_message.Message):
    __slots__ = ("cluster", "max_gross_frac")
    CLUSTER_FIELD_NUMBER: _ClassVar[int]
    MAX_GROSS_FRAC_FIELD_NUMBER: _ClassVar[int]
    cluster: str
    max_gross_frac: float
    def __init__(self, cluster: _Optional[str] = ..., max_gross_frac: _Optional[float] = ...) -> None: ...

class TourneyEstimate(_message.Message):
    __slots__ = ("p_first", "p_top3", "field_size", "threshold_t_p50", "threshold_t_p90", "scenario")
    P_FIRST_FIELD_NUMBER: _ClassVar[int]
    P_TOP3_FIELD_NUMBER: _ClassVar[int]
    FIELD_SIZE_FIELD_NUMBER: _ClassVar[int]
    THRESHOLD_T_P50_FIELD_NUMBER: _ClassVar[int]
    THRESHOLD_T_P90_FIELD_NUMBER: _ClassVar[int]
    SCENARIO_FIELD_NUMBER: _ClassVar[int]
    p_first: float
    p_top3: float
    field_size: int
    threshold_t_p50: float
    threshold_t_p90: float
    scenario: str
    def __init__(self, p_first: _Optional[float] = ..., p_top3: _Optional[float] = ..., field_size: _Optional[int] = ..., threshold_t_p50: _Optional[float] = ..., threshold_t_p90: _Optional[float] = ..., scenario: _Optional[str] = ...) -> None: ...

class TargetPointer(_message.Message):
    __slots__ = ("header", "file", "content_sha256")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    FILE_FIELD_NUMBER: _ClassVar[int]
    CONTENT_SHA256_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    file: str
    content_sha256: str
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., file: _Optional[str] = ..., content_sha256: _Optional[str] = ...) -> None: ...
