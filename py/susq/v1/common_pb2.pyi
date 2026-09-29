from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class SchemaVersion(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SCHEMA_VERSION_UNSPECIFIED: _ClassVar[SchemaVersion]
    SCHEMA_VERSION_CURRENT: _ClassVar[SchemaVersion]

class Side(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SIDE_UNSPECIFIED: _ClassVar[Side]
    SIDE_YES: _ClassVar[Side]
    SIDE_NO: _ClassVar[Side]

class Action(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    ACTION_UNSPECIFIED: _ClassVar[Action]
    ACTION_BUY: _ClassVar[Action]
    ACTION_SELL: _ClassVar[Action]
SCHEMA_VERSION_UNSPECIFIED: SchemaVersion
SCHEMA_VERSION_CURRENT: SchemaVersion
SIDE_UNSPECIFIED: Side
SIDE_YES: Side
SIDE_NO: Side
ACTION_UNSPECIFIED: Action
ACTION_BUY: Action
ACTION_SELL: Action

class Producer(_message.Message):
    __slots__ = ("name", "git_sha")
    NAME_FIELD_NUMBER: _ClassVar[int]
    GIT_SHA_FIELD_NUMBER: _ClassVar[int]
    name: str
    git_sha: str
    def __init__(self, name: _Optional[str] = ..., git_sha: _Optional[str] = ...) -> None: ...

class Header(_message.Message):
    __slots__ = ("schema_version", "produced_at_ns", "producer", "seq")
    SCHEMA_VERSION_FIELD_NUMBER: _ClassVar[int]
    PRODUCED_AT_NS_FIELD_NUMBER: _ClassVar[int]
    PRODUCER_FIELD_NUMBER: _ClassVar[int]
    SEQ_FIELD_NUMBER: _ClassVar[int]
    schema_version: int
    produced_at_ns: int
    producer: Producer
    seq: int
    def __init__(self, schema_version: _Optional[int] = ..., produced_at_ns: _Optional[int] = ..., producer: _Optional[_Union[Producer, _Mapping]] = ..., seq: _Optional[int] = ...) -> None: ...
