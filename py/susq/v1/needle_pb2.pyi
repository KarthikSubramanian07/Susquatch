from susq.v1 import bus_pb2 as _bus_pb2
from susq.v1 import common_pb2 as _common_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class NeedlePrior(_message.Message):
    __slots__ = ("header", "races", "counties", "national_shift_prior_sd")
    HEADER_FIELD_NUMBER: _ClassVar[int]
    RACES_FIELD_NUMBER: _ClassVar[int]
    COUNTIES_FIELD_NUMBER: _ClassVar[int]
    NATIONAL_SHIFT_PRIOR_SD_FIELD_NUMBER: _ClassVar[int]
    header: _common_pb2.Header
    races: _containers.RepeatedCompositeFieldContainer[RacePrior]
    counties: _containers.RepeatedCompositeFieldContainer[CountyPrior]
    national_shift_prior_sd: float
    def __init__(self, header: _Optional[_Union[_common_pb2.Header, _Mapping]] = ..., races: _Optional[_Iterable[_Union[RacePrior, _Mapping]]] = ..., counties: _Optional[_Iterable[_Union[CountyPrior, _Mapping]]] = ..., national_shift_prior_sd: _Optional[float] = ...) -> None: ...

class RacePrior(_message.Message):
    __slots__ = ("race_id", "state", "prior_margin_mean", "prior_margin_sd", "national_beta", "independent_major", "poll_close_utc")
    RACE_ID_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    PRIOR_MARGIN_MEAN_FIELD_NUMBER: _ClassVar[int]
    PRIOR_MARGIN_SD_FIELD_NUMBER: _ClassVar[int]
    NATIONAL_BETA_FIELD_NUMBER: _ClassVar[int]
    INDEPENDENT_MAJOR_FIELD_NUMBER: _ClassVar[int]
    POLL_CLOSE_UTC_FIELD_NUMBER: _ClassVar[int]
    race_id: str
    state: str
    prior_margin_mean: float
    prior_margin_sd: float
    national_beta: float
    independent_major: bool
    poll_close_utc: str
    def __init__(self, race_id: _Optional[str] = ..., state: _Optional[str] = ..., prior_margin_mean: _Optional[float] = ..., prior_margin_sd: _Optional[float] = ..., national_beta: _Optional[float] = ..., independent_major: _Optional[bool] = ..., poll_close_utc: _Optional[str] = ...) -> None: ...

class CountyPrior(_message.Message):
    __slots__ = ("race_id", "state", "county_fips", "expected_total_votes", "expected_total_sd_frac", "prior_margin", "prior_margin_sd", "vote_types")
    RACE_ID_FIELD_NUMBER: _ClassVar[int]
    STATE_FIELD_NUMBER: _ClassVar[int]
    COUNTY_FIPS_FIELD_NUMBER: _ClassVar[int]
    EXPECTED_TOTAL_VOTES_FIELD_NUMBER: _ClassVar[int]
    EXPECTED_TOTAL_SD_FRAC_FIELD_NUMBER: _ClassVar[int]
    PRIOR_MARGIN_FIELD_NUMBER: _ClassVar[int]
    PRIOR_MARGIN_SD_FIELD_NUMBER: _ClassVar[int]
    VOTE_TYPES_FIELD_NUMBER: _ClassVar[int]
    race_id: str
    state: str
    county_fips: str
    expected_total_votes: float
    expected_total_sd_frac: float
    prior_margin: float
    prior_margin_sd: float
    vote_types: _containers.RepeatedCompositeFieldContainer[VoteTypeShare]
    def __init__(self, race_id: _Optional[str] = ..., state: _Optional[str] = ..., county_fips: _Optional[str] = ..., expected_total_votes: _Optional[float] = ..., expected_total_sd_frac: _Optional[float] = ..., prior_margin: _Optional[float] = ..., prior_margin_sd: _Optional[float] = ..., vote_types: _Optional[_Iterable[_Union[VoteTypeShare, _Mapping]]] = ...) -> None: ...

class VoteTypeShare(_message.Message):
    __slots__ = ("vote_type", "share_of_total", "margin_offset", "typical_report_order")
    VOTE_TYPE_FIELD_NUMBER: _ClassVar[int]
    SHARE_OF_TOTAL_FIELD_NUMBER: _ClassVar[int]
    MARGIN_OFFSET_FIELD_NUMBER: _ClassVar[int]
    TYPICAL_REPORT_ORDER_FIELD_NUMBER: _ClassVar[int]
    vote_type: _bus_pb2.VoteType
    share_of_total: float
    margin_offset: float
    typical_report_order: float
    def __init__(self, vote_type: _Optional[_Union[_bus_pb2.VoteType, str]] = ..., share_of_total: _Optional[float] = ..., margin_offset: _Optional[float] = ..., typical_report_order: _Optional[float] = ...) -> None: ...
