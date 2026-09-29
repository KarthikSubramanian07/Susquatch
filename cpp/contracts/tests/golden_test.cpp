// C++ side of the cross-language golden tests: decode the Python-written JSON and binary fixtures,
// require field-for-field equality and byte-identical re-encoding.
#include <fstream>
#include <memory>
#include <sstream>
#include <string>

#include <google/protobuf/util/message_differencer.h>
#include <gtest/gtest.h>

#include "susq/contracts.hpp"

namespace {

std::string Slurp(const std::string& name) {
  std::ifstream f(std::string(SUSQ_FIXTURES_DIR) + "/" + name, std::ios::binary);
  EXPECT_TRUE(f.good()) << "missing fixture " << name;
  std::stringstream ss;
  ss << f.rdbuf();
  return ss.str();
}

template <typename M>
void RoundTrip(const std::string& name) {
  const std::string bin = Slurp(name + ".binpb");
  const std::string json = Slurp(name + ".json");
  M from_bin, from_json;
  ASSERT_TRUE(susq::contracts::Decode(bin, &from_bin)) << name;
  ASSERT_TRUE(susq::contracts::FromJson(json, &from_json)) << name;
  EXPECT_TRUE(google::protobuf::util::MessageDifferencer::Equals(from_bin, from_json)) << name;
  EXPECT_EQ(susq::contracts::Encode(from_bin), bin) << name << ": C++ bytes differ from Python";
  auto ours = susq::contracts::ToJson(from_bin);
  ASSERT_TRUE(ours.has_value());
  M back;
  ASSERT_TRUE(susq::contracts::FromJson(*ours, &back));
  EXPECT_TRUE(google::protobuf::util::MessageDifferencer::Equals(back, from_bin)) << name;
}

}  // namespace

using namespace susq::v1;  // NOLINT

TEST(Golden, TargetFile) { RoundTrip<TargetFile>("target_file"); }
TEST(Golden, TargetFileEmpty) { RoundTrip<TargetFile>("target_file_empty"); }
TEST(Golden, TargetPointer) { RoundTrip<TargetPointer>("target_pointer"); }
TEST(Golden, CountyResultsBatch) { RoundTrip<CountyResultsBatch>("county_results_batch"); }
TEST(Golden, CountyResultsBatchEmpty) { RoundTrip<CountyResultsBatch>("county_results_batch_empty"); }
TEST(Golden, RacePosteriors) { RoundTrip<RacePosteriors>("race_posteriors"); }
TEST(Golden, NationalShift) { RoundTrip<NationalShift>("national_shift"); }
TEST(Golden, Heartbeat) { RoundTrip<Heartbeat>("heartbeat"); }
TEST(Golden, Kill) { RoundTrip<Kill>("kill"); }
TEST(Golden, JournalEntry) { RoundTrip<JournalEntry>("journal_entry"); }
TEST(Golden, OrderIntent) { RoundTrip<OrderIntent>("order_intent"); }
TEST(Golden, TraderLease) { RoundTrip<TraderLease>("trader_lease"); }
TEST(Golden, StatusSnapshot) { RoundTrip<StatusSnapshot>("status_snapshot"); }
TEST(Golden, MarketMap) { RoundTrip<MarketMap>("market_map"); }

TEST(Golden, UnknownFieldTolerated) {
  Heartbeat with_unknown, known;
  ASSERT_TRUE(susq::contracts::Decode(Slurp("heartbeat_unknown_field.binpb"), &with_unknown));
  ASSERT_TRUE(susq::contracts::Decode(Slurp("heartbeat.binpb"), &known));
  with_unknown.DiscardUnknownFields();
  EXPECT_TRUE(google::protobuf::util::MessageDifferencer::Equals(with_unknown, known));
}

TEST(Golden, HeaderVersion) {
  auto h = susq::contracts::MakeHeader("needle", "abc", 1, 0);
  EXPECT_TRUE(susq::contracts::HeaderOk(h));
  h.set_schema_version(99);
  EXPECT_FALSE(susq::contracts::HeaderOk(h));
}
