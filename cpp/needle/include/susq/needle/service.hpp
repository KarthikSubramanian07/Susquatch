// Transport-independent core of the `susq-needle` service: validates bus inputs, drives the
// model and stamps outputs with headers (LANGUAGES_AND_BOUNDARIES §One shared format).
#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

#include "susq/contracts.hpp"
#include "susq/needle/model.hpp"

namespace susq::needle {

inline constexpr std::string_view kSubjectCountyResults = "enr.county_results";
inline constexpr std::string_view kSubjectRacePosteriors = "needle.race_posteriors";
inline constexpr std::string_view kSubjectNationalShift = "needle.national_shift";
inline constexpr std::string_view kSubjectHeartbeat = "needle.heartbeat";

struct ServiceOptions {
  std::string producer = "needle";
  std::string git_sha = "unknown";
  /// First seq on every subject. The binary uses wall-clock microseconds at start-up so seq keeps
  /// increasing across restarts (a restart shows up downstream as a gap, which is intended).
  uint64_t seq_base = 1;
  /// Heartbeats report "degraded:rejected_inputs" for this long after a dropped message.
  int64_t degraded_window_ns = 10'000'000'000;
};

struct ServiceCounters {
  uint64_t batches_ok = 0;
  uint64_t malformed = 0;    ///< undecodable payloads
  uint64_t bad_header = 0;   ///< missing header or foreign schema_version
};

struct BatchOutputs {
  susq::v1::RacePosteriors race_posteriors;
  susq::v1::NationalShift national_shift;
};

class NeedleService {
 public:
  NeedleService(NeedleModel model, ServiceOptions options);

  /// Decodes a binary CountyResultsBatch. Returns the outputs to publish, or nullopt when the
  /// message is dropped (counted in counters()).
  std::optional<BatchOutputs> HandleBatchBytes(std::string_view bytes, int64_t now_ns);

  /// Same as HandleBatchBytes for an already-decoded batch (replay mode).
  std::optional<BatchOutputs> HandleBatch(const susq::v1::CountyResultsBatch& batch, int64_t now_ns);

  /// The next heartbeat message.
  susq::v1::Heartbeat MakeHeartbeat(int64_t now_ns);

  /// Transport health, reflected in heartbeat status.
  void SetTransportHealthy(bool healthy) { transport_healthy_ = healthy; }

  const ServiceCounters& counters() const { return counters_; }
  const NeedleModel& model() const { return model_; }
  uint64_t last_input_seq() const { return last_input_seq_; }

 private:
  void Drop(int64_t now_ns);

  NeedleModel model_;
  ServiceOptions options_;
  ServiceCounters counters_;
  uint64_t last_input_seq_ = 0;
  uint64_t seq_posteriors_;
  uint64_t seq_national_;
  uint64_t seq_heartbeat_;
  int64_t last_drop_ns_ = 0;
  bool dropped_any_ = false;
  bool transport_healthy_ = true;
};

/// UTC nanoseconds since the Unix epoch.
int64_t NowNs();

/// Parses a NeedlePrior from canonical proto3 JSON; throws std::invalid_argument on failure,
/// on a missing header or on a foreign schema_version.
susq::v1::NeedlePrior ParsePrior(std::string_view json);

/// Reads and parses a NeedlePrior JSON file; throws std::runtime_error / std::invalid_argument.
susq::v1::NeedlePrior LoadPrior(const std::string& path);

}  // namespace susq::needle
