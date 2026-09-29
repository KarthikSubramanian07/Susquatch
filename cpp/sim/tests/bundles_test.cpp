#include <gtest/gtest.h>

#include <cmath>
#include <stdexcept>

#include "susq/sim/simulator.hpp"
#include "test_util.hpp"

namespace susq::sim {
namespace {

using testing::BaseConfig;
using testing::Race;

class Bundles : public ::testing::Test {
 protected:
  void SetUp() override {
    cfg_ = BaseConfig(100'000, 77);
    cfg_.clusters = {{"red-state-reach", 2.0}, {"midwest-industrial", 1.5}};
    RaceSpec tx = Race("tx", "TX");
    tx.clusters = {{"red-state-reach", 1.0}};
    tx.target_p_win = 0.6;
    tx.chamber = Chamber::kSenate;
    RaceSpec mi = Race("mi", "MI");
    mi.clusters = {{"midwest-industrial", 1.0}};
    mi.target_p_win = 0.7;
    mi.chamber = Chamber::kSenate;
    RaceSpec me = Race("me", "ME");
    me.target_p_win = 0.55;
    me.chamber = Chamber::kSenate;
    cfg_.races = {tx, mi, me};
    cfg_.senate.dem_seats_not_up = 49;
    w_ = Generate(cfg_);
    (void)Calibrate(cfg_, w_);
    res_ = Evaluate(cfg_, w_);
    b_ = BuildScenarioBundles(cfg_, w_, res_, {"red-state-reach", "midwest-industrial"});
  }
  SimConfig cfg_;
  WorldSet w_;
  Results res_;
  ScenarioBundles b_;
};

TEST_F(Bundles, ShapeAndProbabilitiesSumToOne) {
  ASSERT_EQ(b_.buckets.size(), 40U);
  ASSERT_EQ(b_.decile_edges.size(), 9U);
  double total = 0.0;
  for (const auto& s : b_.buckets) total += s.probability;
  EXPECT_NEAR(total, 1.0, 1e-12);
  for (int dec = 0; dec < 10; ++dec) {
    double p = 0.0;
    for (uint32_t signs = 0; signs < 4; ++signs) p += b_.buckets[dec * 4 + signs].probability;
    EXPECT_NEAR(p, 0.1, 0.002) << "decile " << dec;
  }
}

TEST_F(Bundles, LawOfTotalProbabilityHolds) {
  for (std::size_t i = 0; i < cfg_.races.size(); ++i) {
    double p = 0.0;
    for (const auto& s : b_.buckets) {
      if (s.n_draws > 0) p += s.probability * s.p_dem_market[i];
    }
    EXPECT_NEAR(p, res_.p_dem_market[i], 1e-9);
  }
  double senate = 0.0;
  for (const auto& s : b_.buckets) {
    if (s.n_draws > 0) senate += s.probability * s.p_senate_dem_control;
  }
  EXPECT_NEAR(senate, res_.p_senate_dem_control, 1e-9);
}

TEST_F(Bundles, ConditionalProbabilitiesFollowTheShocks) {
  // TX loads on red-state-reach (bit 0): P(D | shock > 0) > P(D | shock < 0) in each decile.
  for (int dec = 0; dec < 10; ++dec) {
    EXPECT_GT(b_.buckets[dec * 4 + 1].p_dem_market[0], b_.buckets[dec * 4 + 0].p_dem_market[0]);
    EXPECT_GT(b_.buckets[dec * 4 + 2].p_dem_market[1], b_.buckets[dec * 4 + 0].p_dem_market[1]);
  }
  // National deciles are ordered.
  EXPECT_LT(b_.buckets[0].mean_national, b_.buckets[36].mean_national);
  EXPECT_LT(b_.buckets[0].p_dem_market[2], b_.buckets[36].p_dem_market[2]);
}

TEST_F(Bundles, DrawBucketsAreConsistent) {
  std::vector<std::size_t> counts(b_.buckets.size(), 0);
  for (uint32_t bucket : b_.draw_bucket) ++counts[bucket];
  for (std::size_t k = 0; k < counts.size(); ++k) EXPECT_EQ(counts[k], b_.buckets[k].n_draws);
}

TEST_F(Bundles, RejectsUnknownCluster) {
  EXPECT_THROW(BuildScenarioBundles(cfg_, w_, res_, {"nope"}), std::invalid_argument);
}

}  // namespace
}  // namespace susq::sim
