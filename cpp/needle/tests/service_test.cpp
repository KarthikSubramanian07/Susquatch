#include <gtest/gtest.h>

#include <fstream>
#include <sstream>
#include <string>

#include "susq/contracts.hpp"
#include "susq/needle/service.hpp"

namespace susq::needle {
namespace {

std::string ReadFile(const std::string& path) {
  std::ifstream in(path, std::ios::binary);
  std::ostringstream buf;
  buf << in.rdbuf();
  return buf.str();
}

NeedleService MakeService() {
  ServiceOptions opts;
  opts.git_sha = "abc123";
  opts.seq_base = 100;
  opts.degraded_window_ns = 1'000;
  return NeedleService(NeedleModel(LoadPrior(std::string(SUSQ_FIXTURES_DIR) + "/needle_prior.json")), opts);
}

TEST(Service, HandlesTheGoldenBatch) {
  NeedleService svc = MakeService();
  const std::string bytes = ReadFile(std::string(SUSQ_FIXTURES_DIR) + "/county_results_batch.binpb");
  auto out = svc.HandleBatchBytes(bytes, 5'000);
  ASSERT_TRUE(out.has_value());
  EXPECT_EQ(svc.last_input_seq(), 42U);
  // The fixture has one usable OH result and one malformed (no candidates, u64-max total) CA result.
  EXPECT_EQ(svc.model().counters().accepted, 1U);
  EXPECT_EQ(svc.model().counters().unknown_race, 1U);  // CA-13 is not in the fixture prior
  ASSERT_EQ(out->race_posteriors.races_size(), 1);
  const auto& oh = out->race_posteriors.races(0);
  EXPECT_EQ(oh.race_id(), "2026-SEN-OH-S");
  EXPECT_GT(oh.reported_frac(), 0.5);  // mail (above expectation) in, Election Day not
  EXPECT_GE(oh.p_dem(), 0.0);
  EXPECT_LE(oh.p_dem() + oh.p_rep(), 1.0 + 1e-12);
  const auto& h = out->race_posteriors.header();
  EXPECT_EQ(h.schema_version(), susq::contracts::kSchemaVersion);
  EXPECT_EQ(h.producer().name(), "needle");
  EXPECT_EQ(h.producer().git_sha(), "abc123");
  EXPECT_EQ(h.seq(), 100U);
  EXPECT_EQ(h.produced_at_ns(), 5'000);
  EXPECT_EQ(out->national_shift.header().seq(), 100U);
  EXPECT_EQ(out->national_shift.n_races_informing(), 1U);
}

TEST(Service, SeqIncreasesPerSubject) {
  NeedleService svc = MakeService();
  susq::v1::CountyResultsBatch batch;
  *batch.mutable_header() = susq::contracts::MakeHeader("rust-live", "x", 7, 1);
  const auto a = svc.HandleBatch(batch, 10);
  const auto b = svc.HandleBatch(batch, 20);
  ASSERT_TRUE(a && b);
  EXPECT_EQ(b->race_posteriors.header().seq(), a->race_posteriors.header().seq() + 1);
  EXPECT_EQ(b->national_shift.header().seq(), a->national_shift.header().seq() + 1);
  const auto h1 = svc.MakeHeartbeat(30);
  const auto h2 = svc.MakeHeartbeat(40);
  EXPECT_EQ(h1.header().seq(), 100U);  // heartbeat has its own counter
  EXPECT_EQ(h2.header().seq(), 101U);
  EXPECT_EQ(h2.last_input_seq(), 7U);
  EXPECT_EQ(h2.status(), "ok");
}

TEST(Service, DropsMalformedAndForeignSchemaMessages) {
  NeedleService svc = MakeService();
  EXPECT_FALSE(svc.HandleBatchBytes(std::string("\xff\xff\xff\xff", 4), 100).has_value());
  EXPECT_EQ(svc.counters().malformed, 1U);
  EXPECT_EQ(svc.MakeHeartbeat(500).status(), "degraded:rejected_inputs");
  EXPECT_EQ(svc.MakeHeartbeat(5'000).status(), "ok");  // after the degraded window

  susq::v1::CountyResultsBatch no_header;
  EXPECT_FALSE(svc.HandleBatchBytes(susq::contracts::Encode(no_header), 6'000).has_value());
  susq::v1::CountyResultsBatch future;
  *future.mutable_header() = susq::contracts::MakeHeader("rust-live", "x", 1, 1);
  future.mutable_header()->set_schema_version(99);
  EXPECT_FALSE(svc.HandleBatchBytes(susq::contracts::Encode(future), 6'000).has_value());
  EXPECT_EQ(svc.counters().bad_header, 2U);
  EXPECT_EQ(svc.counters().batches_ok, 0U);
  EXPECT_EQ(svc.last_input_seq(), 0U);
}

TEST(Service, ReportsBusOutage) {
  NeedleService svc = MakeService();
  svc.SetTransportHealthy(false);
  EXPECT_EQ(svc.MakeHeartbeat(1).status(), "degraded:bus_disconnected");
  svc.SetTransportHealthy(true);
  EXPECT_EQ(svc.MakeHeartbeat(2).status(), "ok");
}

}  // namespace
}  // namespace susq::needle
