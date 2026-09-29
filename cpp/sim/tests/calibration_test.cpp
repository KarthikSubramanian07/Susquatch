#include <gtest/gtest.h>

#include <cmath>
#include <stdexcept>

#include "susq/sim/simulator.hpp"
#include "test_util.hpp"

namespace susq::sim {
namespace {

using testing::BaseConfig;
using testing::Race;

// Inverse standard-normal CDF by bisection on erfc (test oracle only).
double NormalQuantile(double p) {
  double lo = -10.0;
  double hi = 10.0;
  for (int i = 0; i < 200; ++i) {
    const double mid = 0.5 * (lo + hi);
    (0.5 * std::erfc(-mid / std::sqrt(2.0)) < p ? lo : hi) = mid;
  }
  return 0.5 * (lo + hi);
}

TEST(Calibration, HitsTargetsWithinATenthOfAPoint) {
  SimConfig cfg = BaseConfig(200'000, 17);
  cfg.clusters = {{"red-state-reach", 2.0}};
  const char* states[] = {"OH", "IA", "ME", "TX", "AK", "KS", "MI", "NH", "SC", "GA", "NC"};
  const double targets[] = {0.58, 0.43, 0.60, 0.61, 0.71, 0.30, 0.72, 0.86, 0.125, 0.94, 0.03};
  for (int i = 0; i < 11; ++i) {
    RaceSpec r = Race(states[i], states[i], 2.5 + 0.1 * i);
    r.target_p_win = targets[i];
    if (i % 3 == 0) r.clusters = {{"red-state-reach", 1.0}};
    cfg.races.push_back(r);
  }
  const WorldSet w = Generate(cfg);
  const CalibrationReport rep = Calibrate(cfg, w);
  ASSERT_EQ(rep.entries.size(), 11U);
  EXPECT_LT(rep.max_abs_error, 0.001);
  const Results res = Evaluate(cfg, w);
  for (std::size_t i = 0; i < rep.entries.size(); ++i) {
    EXPECT_TRUE(rep.entries[i].feasible);
    EXPECT_NEAR(res.p_dem_side[i], targets[i], 0.001) << states[i];
    EXPECT_DOUBLE_EQ(res.p_dem_side[i], rep.entries[i].achieved);
    EXPECT_DOUBLE_EQ(cfg.races[i].mu, rep.entries[i].mu);
  }
}

TEST(Calibration, MatchesGaussianQuantileWhenShocksAreGaussian) {
  SimConfig cfg = BaseConfig(400'000, 21);
  cfg.shocks.national_nu = 1e6;  // t -> normal
  cfg.shocks.miss_prob = 0.0;
  RaceSpec r = Race("oh", "OH", 3.0);
  r.target_p_win = 0.7;
  cfg.races = {r};
  const WorldSet w = Generate(cfg);
  (void)Calibrate(cfg, w);
  const auto& s = cfg.shocks;
  const double sd = std::sqrt(s.national_sd * s.national_sd + s.region_sd * s.region_sd +
                              s.state_sd * s.state_sd + 9.0);
  EXPECT_NEAR(cfg.races[0].mu, sd * NormalQuantile(0.7), 0.05);
}

TEST(Calibration, ThirdPartyMassIsRespected) {
  SimConfig cfg = BaseConfig(100'000, 23);
  RaceSpec ok = Race("ne", "NE");
  ok.p_third_party = 0.2;
  ok.target_p_win = 0.5;
  RaceSpec impossible = Race("ne2", "NE");
  impossible.p_third_party = 0.2;
  impossible.target_p_win = 0.9;
  cfg.races = {ok, impossible};
  const WorldSet w = Generate(cfg);
  const CalibrationReport rep = Calibrate(cfg, w);
  EXPECT_TRUE(rep.entries[0].feasible);
  EXPECT_NEAR(rep.entries[0].achieved, 0.5, 0.001);
  EXPECT_FALSE(rep.entries[1].feasible);
  EXPECT_NEAR(rep.entries[1].achieved, 0.8, 0.01);  // capped at 1 - realized third-party share
  const Results res = Evaluate(cfg, w);
  EXPECT_NEAR(res.p_rep_market[0], 0.3, 0.01);
}

TEST(Calibration, HigherTargetMeansHigherMean) {
  SimConfig cfg = BaseConfig(50'000, 29);
  RaceSpec a = Race("a", "OH");
  a.target_p_win = 0.4;
  RaceSpec b = Race("b", "OH");
  b.target_p_win = 0.6;
  cfg.races = {a, b};
  const WorldSet w = Generate(cfg);
  (void)Calibrate(cfg, w);
  EXPECT_LT(cfg.races[0].mu, cfg.races[1].mu);
}

TEST(Calibration, RacesWithoutTargetKeepTheirMean) {
  SimConfig cfg = BaseConfig(10'000, 31);
  RaceSpec fixed = Race("fixed", "OH");
  fixed.mu = 4.25;
  cfg.races = {fixed};
  const WorldSet w = Generate(cfg);
  const CalibrationReport rep = Calibrate(cfg, w);
  EXPECT_TRUE(rep.entries.empty());
  EXPECT_DOUBLE_EQ(cfg.races[0].mu, 4.25);
}

TEST(Calibration, RejectsMismatchedWorldSet) {
  SimConfig cfg = BaseConfig(1000, 1);
  cfg.races = {Race("a", "OH")};
  const WorldSet w = Generate(cfg);
  cfg.races.push_back(Race("b", "GA"));
  EXPECT_THROW(Calibrate(cfg, w), std::invalid_argument);
  EXPECT_THROW(Evaluate(cfg, w), std::invalid_argument);
}

}  // namespace
}  // namespace susq::sim
