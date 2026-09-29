#include "susq/needle/service.hpp"

#include <chrono>
#include <fstream>
#include <sstream>
#include <stdexcept>
#include <utility>

namespace susq::needle {

int64_t NowNs() {
  return std::chrono::duration_cast<std::chrono::nanoseconds>(
             std::chrono::system_clock::now().time_since_epoch())
      .count();
}

susq::v1::NeedlePrior ParsePrior(std::string_view json) {
  susq::v1::NeedlePrior prior;
  if (!susq::contracts::FromJson(json, &prior)) throw std::invalid_argument("needle prior: not valid NeedlePrior JSON");
  if (!prior.has_header() || !susq::contracts::HeaderOk(prior.header())) {
    throw std::invalid_argument("needle prior: missing header or unsupported schema_version");
  }
  return prior;
}

susq::v1::NeedlePrior LoadPrior(const std::string& path) {
  std::ifstream in(path, std::ios::binary);
  if (!in) throw std::runtime_error("needle prior: cannot open " + path);
  std::ostringstream buf;
  buf << in.rdbuf();
  return ParsePrior(buf.str());
}

NeedleService::NeedleService(NeedleModel model, ServiceOptions options)
    : model_(std::move(model)),
      options_(std::move(options)),
      seq_posteriors_(options_.seq_base),
      seq_national_(options_.seq_base),
      seq_heartbeat_(options_.seq_base) {}

void NeedleService::Drop(int64_t now_ns) {
  dropped_any_ = true;
  last_drop_ns_ = now_ns;
}

std::optional<BatchOutputs> NeedleService::HandleBatchBytes(std::string_view bytes, int64_t now_ns) {
  susq::v1::CountyResultsBatch batch;
  if (!susq::contracts::Decode(bytes, &batch)) {
    ++counters_.malformed;
    Drop(now_ns);
    return std::nullopt;
  }
  return HandleBatch(batch, now_ns);
}

std::optional<BatchOutputs> NeedleService::HandleBatch(const susq::v1::CountyResultsBatch& batch,
                                                       int64_t now_ns) {
  if (!batch.has_header() || !susq::contracts::HeaderOk(batch.header())) {
    ++counters_.bad_header;
    Drop(now_ns);
    return std::nullopt;
  }
  model_.IngestBatch(batch);
  ++counters_.batches_ok;
  last_input_seq_ = batch.header().seq();

  BatchOutputs out;
  model_.FillRacePosteriors(&out.race_posteriors);
  *out.race_posteriors.mutable_header() =
      susq::contracts::MakeHeader(options_.producer, options_.git_sha, seq_posteriors_++, now_ns);
  model_.FillNationalShift(&out.national_shift);
  *out.national_shift.mutable_header() =
      susq::contracts::MakeHeader(options_.producer, options_.git_sha, seq_national_++, now_ns);
  return out;
}

susq::v1::Heartbeat NeedleService::MakeHeartbeat(int64_t now_ns) {
  susq::v1::Heartbeat hb;
  *hb.mutable_header() = susq::contracts::MakeHeader(options_.producer, options_.git_sha, seq_heartbeat_++, now_ns);
  if (!transport_healthy_) {
    hb.set_status("degraded:bus_disconnected");
  } else if (dropped_any_ && now_ns - last_drop_ns_ < options_.degraded_window_ns) {
    hb.set_status("degraded:rejected_inputs");
  } else {
    hb.set_status("ok");
  }
  hb.set_last_input_seq(last_input_seq_);
  return hb;
}

}  // namespace susq::needle
