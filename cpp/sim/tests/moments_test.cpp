// Each shock component of e = beta*N + R_region + S_state + sum(loading*C) + eps matches its
// parameters, identified through variances and cross-race covariances.
#include <gtest/gtest.h>

#include <cmath>
#include <cstring>

#include "susq/sim/simulator.hpp"
#include "test_util.hpp"

namespace susq::sim {
namespace {

using testing::BaseConfig;
using testing::Corr;
using testing::Cov;
using testing::Mean;
using testing::NationalVariance;
using testing::Race;
using testing::RaceErrors;
using testing::Var;

constexpr std::size_t kDraws = 400'000;

TEST(Moments, NationalShiftIsScaledStudentT) {
  SimConfig cfg = BaseConfig(kDraws);
  cfg.shocks.miss_prob = 0.0;
  cfg.shocks.national_nu = 10.0;
  cfg.races.push_back(Race("a", "OH"));
  const WorldSet w = Generate(cfg);
  const float* n = w.national.data();
  EXPECT_NEAR(Mean(n, kDraws), 0.0, 0.02);
  EXPECT_NEAR(std::sqrt(Var(n, kDraws)), cfg.shocks.national_sd, 0.02);
  double m4 = 0.0;
  const double v = Var(n, kDraws);
  for (std::size_t d = 0; d < kDraws; ++d) m4 += std::pow(n[d], 4);
  EXPECT_NEAR(m4 / kDraws / (v * v) - 3.0, 6.0 / (10.0 - 4.0), 0.3);
}

TEST(Moments, SystematicMissMixtureProbabilityAndSign) {
  for (double p_dem : {0.5, 0.8}) {
    SimConfig cfg = BaseConfig(kDraws);
    cfg.shocks.national_sd = 1e-6;
    cfg.shocks.miss_prob = 0.1;
    cfg.shocks.miss_size = 3.5;
    cfg.shocks.miss_prob_dem = p_dem;
    cfg.races.push_back(Race("a", "OH"));
    const WorldSet w = Generate(cfg);
    std::size_t misses = 0;
    std::size_t dem = 0;
    for (float x : w.national) {
      if (std::abs(x) > 1.0F) {
        ++misses;
        EXPECT_NEAR(std::abs(x), 3.5, 1e-3);
        dem += x > 0 ? 1 : 0;
      }
    }
    EXPECT_NEAR(static_cast<double>(misses) / kDraws, 0.1, 0.003);
    EXPECT_NEAR(static_cast<double>(dem) / static_cast<double>(misses), p_dem, 0.01);
  }
}

TEST(Moments, NationalVarianceIncludesMixture) {
  SimConfig cfg = BaseConfig(kDraws);  // SPEC defaults: sd 2.75, nu 5, p 0.1, +-3.5
  cfg.races.push_back(Race("a", "OH"));
  const WorldSet w = Generate(cfg);
  EXPECT_NEAR(Var(w.national.data(), kDraws), NationalVariance(cfg.shocks), 0.25);
}

// Four races: two in OH (same state), MI (same region), GA (different region).
class ComponentCovariance : public ::testing::Test {
 protected:
  void SetUp() override {
    cfg_ = BaseConfig(kDraws, 99);
    cfg_.races = {Race("oh1", "OH", 3.0), Race("oh2", "OH", 3.5), Race("mi", "MI", 2.5),
                  Race("ga", "GA", 3.0)};
    w_ = Generate(cfg_);
    vn_ = NationalVariance(cfg_.shocks);
  }
  double CovRaces(std::size_t i, std::size_t j) const {
    return Cov(RaceErrors(w_, i), RaceErrors(w_, j), kDraws);
  }
  SimConfig cfg_;
  WorldSet w_;
  double vn_ = 0.0;
};

TEST_F(ComponentCovariance, TotalVariancePerRace) {
  const double r2 = std::pow(cfg_.shocks.region_sd, 2);
  const double s2 = std::pow(cfg_.shocks.state_sd, 2);
  for (std::size_t i = 0; i < cfg_.races.size(); ++i) {
    const double expected = vn_ + r2 + s2 + std::pow(cfg_.races[i].sd, 2);
    EXPECT_NEAR(CovRaces(i, i), expected, 0.03 * expected) << cfg_.races[i].id;
  }
}

TEST_F(ComponentCovariance, SharedComponentsIdentifiedByCovariance) {
  const double r2 = std::pow(cfg_.shocks.region_sd, 2);
  const double s2 = std::pow(cfg_.shocks.state_sd, 2);
  EXPECT_NEAR(CovRaces(0, 1), vn_ + r2 + s2, 0.3);  // same state
  EXPECT_NEAR(CovRaces(0, 2), vn_ + r2, 0.3);       // same region only
  EXPECT_NEAR(CovRaces(0, 3), vn_, 0.3);            // national only
}

TEST_F(ComponentCovariance, SameStateCorrelationExceedsCrossRegion) {
  const double same_state = Corr(RaceErrors(w_, 0), RaceErrors(w_, 1), kDraws);
  const double same_region = Corr(RaceErrors(w_, 0), RaceErrors(w_, 2), kDraws);
  const double cross_region = Corr(RaceErrors(w_, 0), RaceErrors(w_, 3), kDraws);
  EXPECT_GT(same_state, same_region + 0.05);
  EXPECT_GT(same_region, cross_region + 0.02);
  EXPECT_GT(cross_region, 0.1);
}

TEST(Moments, ClusterShocksAndLoadings) {
  SimConfig cfg = BaseConfig(kDraws, 5);
  cfg.clusters = {{"red-state-reach", 2.0}};
  RaceSpec tx = Race("tx", "TX");
  tx.clusters = {{"red-state-reach", 1.0}};
  RaceSpec ak = Race("ak", "AK");
  ak.clusters = {{"red-state-reach", 0.5}};
  RaceSpec ny = Race("ny", "NY");
  cfg.races = {tx, ak, ny};
  const WorldSet w = Generate(cfg);
  const double vn = NationalVariance(cfg.shocks);
  EXPECT_NEAR(Var(w.cluster_shocks.data(), kDraws), 4.0, 0.08);
  EXPECT_NEAR(Cov(RaceErrors(w, 0), RaceErrors(w, 1), kDraws), vn + 0.5 * 4.0, 0.3);
  EXPECT_NEAR(Cov(RaceErrors(w, 0), RaceErrors(w, 2), kDraws), vn, 0.3);
}

TEST(Moments, NationalBetaScalesNationalLoading) {
  SimConfig cfg = BaseConfig(kDraws, 6);
  RaceSpec a = Race("a", "OH");
  a.national_beta = 0.5;
  cfg.races = {a, Race("b", "GA")};
  const WorldSet w = Generate(cfg);
  EXPECT_NEAR(Cov(RaceErrors(w, 0), RaceErrors(w, 1), kDraws), 0.5 * NationalVariance(cfg.shocks), 0.25);
}

TEST(Moments, StatelessRaceHasNoRegionOrStateShock) {
  SimConfig cfg = BaseConfig(kDraws, 9);
  cfg.races = {Race("house-control", "", 3.0), Race("oh", "OH")};
  const WorldSet w = Generate(cfg);
  const double vn = NationalVariance(cfg.shocks);
  EXPECT_NEAR(Var(RaceErrors(w, 0), kDraws), vn + 9.0, 0.03 * (vn + 9.0));
  EXPECT_NEAR(Cov(RaceErrors(w, 0), RaceErrors(w, 1), kDraws), vn, 0.3);
}

TEST(Moments, ThirdPartyFrequency) {
  SimConfig cfg = BaseConfig(kDraws, 8);
  RaceSpec ne = Race("ne", "NE");
  ne.p_third_party = 0.3;
  cfg.races = {ne, Race("oh", "OH")};
  const WorldSet w = Generate(cfg);
  ASSERT_EQ(w.third_party[0].size(), kDraws);
  EXPECT_TRUE(w.third_party[1].empty());
  double hits = 0;
  for (uint8_t t : w.third_party[0]) hits += t;
  EXPECT_NEAR(hits / kDraws, 0.3, 0.003);
}

TEST(Determinism, IdenticalAcrossThreadCounts) {
  SimConfig cfg = BaseConfig(50'001, 42);  // not a multiple of the block size
  cfg.clusters = {{"midwest-industrial", 1.5}};
  RaceSpec mi = Race("mi", "MI");
  mi.clusters = {{"midwest-industrial", 1.0}};
  RaceSpec ne = Race("ne", "NE");
  ne.p_third_party = 0.2;
  cfg.races = {mi, ne, Race("ga", "GA")};
  for (int j = 0; j < 20; ++j) {
    HouseDistrictSpec d;
    d.id = "D" + std::to_string(j);
    d.state = j % 2 ? "CA" : "PA";
    d.lean = -10.0 + j;
    cfg.districts.push_back(d);
  }
  cfg.threads = 1;
  const WorldSet one = Generate(cfg);
  for (unsigned t : {2U, 3U, 8U}) {
    cfg.threads = t;
    const WorldSet many = Generate(cfg);
    EXPECT_EQ(one.national, many.national) << t;
    EXPECT_EQ(one.race_errors, many.race_errors) << t;
    EXPECT_EQ(one.cluster_shocks, many.cluster_shocks) << t;
    EXPECT_EQ(one.third_party, many.third_party) << t;
    EXPECT_EQ(one.house_unlinked_dem, many.house_unlinked_dem) << t;
  }
}

TEST(Determinism, AddingARaceLeavesOtherDrawsUnchanged) {
  SimConfig cfg = BaseConfig(20'000, 3);
  cfg.races = {Race("oh", "OH"), Race("ga", "GA")};
  const WorldSet before = Generate(cfg);
  cfg.races.insert(cfg.races.begin(), Race("tx", "TX"));
  const WorldSet after = Generate(cfg);
  EXPECT_EQ(0, std::memcmp(RaceErrors(before, 0), RaceErrors(after, 1), 20'000 * sizeof(float)));
  EXPECT_EQ(0, std::memcmp(RaceErrors(before, 1), RaceErrors(after, 2), 20'000 * sizeof(float)));
}

TEST(Determinism, SeedChangesDraws) {
  SimConfig cfg = BaseConfig(1000, 1);
  cfg.races = {Race("oh", "OH")};
  const WorldSet a = Generate(cfg);
  cfg.seed = 2;
  const WorldSet b = Generate(cfg);
  EXPECT_NE(a.race_errors, b.race_errors);
}

}  // namespace
}  // namespace susq::sim
