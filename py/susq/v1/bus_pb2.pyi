from susq.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class VoteType(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    VOTE_TYPE_UNSPECIFIED: _ClassVar[VoteType]
    VOTE_TYPE_MAIL: _ClassVar[VoteType]
    VOTE_TYPE_EARLY_IN_PERSON: _ClassVar[VoteType]
    VOTE_TYPE_ELECTION_DAY: _ClassVar[VoteType]
    VOTE_TYPE_PROVISIONAL: _ClassVar[VoteType]
    VOTE_TYPE_MIXED: _ClassVar[VoteType]
VOTE_TYPE_UNSPECIFIED: VoteType
VOTE_TYPE_MAIL: VoteType
VOTE_TYPE_EARLY_IN_PERSON: VoteType
VOTE_TYPE_ELECTION_DAY: VoteType
VOTE_TYPE_PROVISIONAL: VoteType
VOTE_TYPE_MIXED: VoteType

class CandidateVotes(_message.Message):
    __slots__ = ("candidate", "party", "votes")
    CANDIDATE_FIELD_NUMBER: _ClassVar[int]
    PARTY_FIELD_NUMBER: _ClassVar[int]
    VOTES_FIELD_NUMBER: _ClassVar[int]
    candidate: str
    party: str
    votes: int
    def __init__(self, candidate: _Optional[str] = ..., party: _Optional[str] = ..., votes: _Optional[int] = ...) -> None: ...

class CountyResult(_message.Message):
    __slots__ = ("race_id", "state", "county_fips", "source", "source_ts_ns", "observed_at_ns", "vote_type", "votes", "total_votes", "precincts_reporting", "precincts_total", "batch_id", "is_correction")
    RACE_ID_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    COUNTY_FIPS_FIELD_NUMBER: _ClassVar[int]
    SOURCE_FIELD_NUMBER: _ClassVar[int]
    SOURCE_TS_NS_FIELD_NUMBER: _ClassVar[int]
    OBSERVED_AT_NS_FIELD_NUMBER: _ClassVar[int]
    VOTE_TYPE_FIELD_NUMBER: _ClassVar[int]
    VOTES_FIELD_NUMBER: _ClassVar[int]
    TOTAL_VOTES_FIELD_NUMBER: _ClassVar[int]
    PRECINCTS_REPORTING_FIELD_NUMBER: _ClassVar[int]
    PRECINCTS_TOTAL_FIELD_NUMBER: _ClassVar[int]
    BATCH_ID_FIELD_NUMBER: _ClassVar[int]
    IS_CORRECTION_FIELD_NUMBER: _ClassVar[int]
    race_id: str
    state: str
    county_fips: str
    source: str
    source_ts_ns: int
    observed_at_ns: int
    vote_type: VoteType
    votes: _containers.RepeatedCompositeFieldContainer[CandidateVotes]
    total_votes: int
    precincts_reporting: int
    precincts_total: int
    batch_id: str
    is_correction: bool
    def __init__(self, race_id: _Optional[str] = ..., state: _Optional[str] = ..., county_fips: _Optional[str] = ..., source: _Optional[str] = ..., source_ts_ns: _Optional[int] = ..., observed_at_ns: _Optional[int] = ..., vote_type: _Optional[_Union[VoteType, str]] = ..., votes: _Optional[_Iterable[_Union[CandidateVotes, _Mapping]]] = ..., total_votes: _Optional[int] = ..., precincts_reporting: _Optional[int] = ..., precincts_total: _Optional[int] = ..., batch_id: _Optional[str] = ..., is_correction: _Optional[bool] = ...) -> None: ...

class CountyResultsBatch(_message.Message):
    __slots__ = ("header", "results")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    RESULTS_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    results: _containers.RepeatedCompositeFieldContainer[CountyResult]
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., results: _Optional[_Iterable[_Union[CountyResult, _Mapping]]] = ...) -> None: ...

class RacePosterior(_message.Message):
    __slots__ = ("race_id", "p_dem", "p_rep", "margin_mean", "margin_sd", "expected_remaining_votes", "reported_frac", "n_batches", "n_counties_reporting")
    RACE_ID_FIELD_NUMBER: _ClassVar[int]
    P_DEM_FIELD_NUMBER: _ClassVar[int]
    P_REP_FIELD_NUMBER: _ClassVar[int]
    MARGIN_MEAN_FIELD_NUMBER: _ClassVar[int]
    MARGIN_SD_FIELD_NUMBER: _ClassVar[int]
    EXPECTED_REMAINING_VOTES_FIELD_NUMBER: _ClassVar[int]
    REPORTED_FRAC_FIELD_NUMBER: _ClassVar[int]
    N_BATCHES_FIELD_NUMBER: _ClassVar[int]
    N_COUNTIES_REPORTING_FIELD_NUMBER: _ClassVar[int]
    race_id: str
    p_dem: float
    p_rep: float
    margin_mean: float
    margin_sd: float
    expected_remaining_votes: float
    reported_frac: float
    n_batches: int
    n_counties_reporting: int
    def __init__(self, race_id: _Optional[str] = ..., p_dem: _Optional[float] = ..., p_rep: _Optional[float] = ..., margin_mean: _Optional[float] = ..., margin_sd: _Optional[float] = ..., expected_remaining_votes: _Optional[float] = ..., reported_frac: _Optional[float] = ..., n_batches: _Optional[int] = ..., n_counties_reporting: _Optional[int] = ...) -> None: ...

class RacePosteriors(_message.Message):
    __slots__ = ("header", "races")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    RACES_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    races: _containers.RepeatedCompositeFieldContainer[RacePosterior]
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., races: _Optional[_Iterable[_Union[RacePosterior, _Mapping]]] = ...) -> None: ...

class NationalShift(_message.Message):
    __slots__ = ("header", "mean", "sd", "n_races_informing")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    MEAN_FIELD_NUMBER: _ClassVar[int]
    SD_FIELD_NUMBER: _ClassVar[int]
    N_RACES_INFORMING_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    mean: float
    sd: float
    n_races_informing: int
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., mean: _Optional[float] = ..., sd: _Optional[float] = ..., n_races_informing: _Optional[int] = ...) -> None: ...

class Heartbeat(_message.Message):
    __slots__ = ("header", "status", "last_input_seq")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    LAST_INPUT_SEQ_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    status: str
    last_input_seq: int
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., status: _Optional[str] = ..., last_input_seq: _Optional[int] = ...) -> None: ...

class Kill(_message.Message):
    __slots__ = ("header", "reason", "issuer")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    REASON_FIELD_NUMBER: _ClassVar[int]
    ISSUER_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    reason: str
    issuer: str
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., reason: _Optional[str] = ..., issuer: _Optional[str] = ...) -> None: ...
