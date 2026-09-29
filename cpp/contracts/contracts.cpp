#include "susq/contracts.hpp"

#include <google/protobuf/io/coded_stream.h>
#include <google/protobuf/io/zero_copy_stream_impl_lite.h>
#include <google/protobuf/util/json_util.h>

namespace susq::contracts {

std::string Encode(const google::protobuf::Message& msg) {
  std::string out;
  bool ok = false;
  {
    google::protobuf::io::StringOutputStream sos(&out);
    google::protobuf::io::CodedOutputStream cos(&sos);
    cos.SetSerializationDeterministic(true);
    ok = msg.SerializeToCodedStream(&cos);
  }
  // A message missing nothing (proto3 has no required fields) cannot fail to serialize; if it ever
  // does, return empty rather than a truncated buffer so the receiver rejects it instead of misreading.
  return ok ? out : std::string();
}

bool Decode(std::string_view bytes, google::protobuf::Message* out) {
  return out->ParseFromArray(bytes.data(), static_cast<int>(bytes.size()));
}

std::optional<std::string> ToJson(const google::protobuf::Message& msg) {
  std::string out;
  google::protobuf::util::JsonPrintOptions opts;
  if (!google::protobuf::util::MessageToJsonString(msg, &out, opts).ok()) return std::nullopt;
  return out;
}

bool FromJson(std::string_view json, google::protobuf::Message* out) {
  google::protobuf::util::JsonParseOptions opts;
  opts.ignore_unknown_fields = true;
  return google::protobuf::util::JsonStringToMessage(json, out, opts).ok();
}

susq::v1::Header MakeHeader(std::string_view producer, std::string_view git_sha, uint64_t seq,
                            int64_t produced_at_ns) {
  susq::v1::Header h;
  h.set_schema_version(kSchemaVersion);
  h.set_produced_at_ns(produced_at_ns);
  h.mutable_producer()->set_name(std::string(producer));
  h.mutable_producer()->set_git_sha(std::string(git_sha));
  h.set_seq(seq);
  return h;
}

bool HeaderOk(const susq::v1::Header& h) { return h.schema_version() == kSchemaVersion; }

}  // namespace susq::contracts
