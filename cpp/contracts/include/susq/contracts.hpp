// Encoding rules shared by every C++ producer/consumer of the susq.v1 contracts.
#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

#include <google/protobuf/message.h>

#include "susq/v1/bus.pb.h"
#include "susq/v1/common.pb.h"
#include "susq/v1/ops.pb.h"
#include "susq/v1/targets.pb.h"

namespace susq::contracts {

inline constexpr uint32_t kSchemaVersion = susq::v1::SCHEMA_VERSION_CURRENT;

// Deterministic binary encoding (matches Python `SerializeToString(deterministic=True)` and prost).
std::string Encode(const google::protobuf::Message& msg);

// Binary decode; unknown fields are preserved but never interpreted. Returns false on malformed input.
bool Decode(std::string_view bytes, google::protobuf::Message* out);

// Canonical proto3 JSON (lowerCamelCase, int64 as strings).
std::optional<std::string> ToJson(const google::protobuf::Message& msg);
bool FromJson(std::string_view json, google::protobuf::Message* out);

// Fill a header with the current schema version.
susq::v1::Header MakeHeader(std::string_view producer, std::string_view git_sha, uint64_t seq,
                            int64_t produced_at_ns);

// True when the header exists and carries our schema version.
bool HeaderOk(const susq::v1::Header& h);

}  // namespace susq::contracts
