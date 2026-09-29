#include <gtest/gtest.h>

#include <algorithm>
#include <cmath>
#include <vector>

#include "susq/tourney/sampled.hpp"

namespace susq::tourney {
namespace {

double Phi(double x) { return 0.5 * std::erfc(-x / std::sqrt(2.0)); }

SampledFieldParams Only(uint32_t n, double kelly, double yolo, double bot) {
  SampledFieldParams p;
  p.n_players = n;
  p.frac_kelly = kelly;
  p.frac_yolo = yolo;
  p.frac_bot = bot;
  return p;
}

double FractionAtMost(const std::vector<double>& t, double m) {
  return static_cast<double>(std::count_if(t.begin(), t.end(), [&](double x) { return x <= m; })) /
         static_cast<double>(t.size());
}

TEST(NormalQuantile, InvertsTheCdf) {
  for (double p : {1e-12, 1e-6, 0.001, 0.02425, 0.1, 0.5, 0.77, 0.975, 0.999999}) {
    EXPECT_NEAR(Phi(NormalQuantile(p)), p, 1e-13 + 1e-10 * p) << p;
  }
  EXPECT_NEAR(NormalQuantile(0.975), 1.959963984540054, 1e-12);
  EXPECT_TRUE(std::isinf(NormalQuantile(0.0)));
}

TEST(SampledThreshold, KellyMaximumMatchesOrderStatistic) {
  const std::size_t worlds = 200'000;
  const std::vector<uint8_t> outcomes(worlds, 0);
  const auto t = SampleThresholds(outcomes.data(), worlds, {}, Only(10, 1.0, 0.0, 0.0), 1);
  for (double m : {1.1, 1.2, 1.25}) EXPECT_NEAR(FractionAtMost(t, m), std::pow((m - 0.8) / 0.5, 10), 0.004) << m;
  EXPECT_NEAR(FractionAtMost(t, 1.0), std::pow(0.4, 10), 1e-4);  // passive floor: T >= 1.0
}

TEST(SampledThreshold, BotMaximumIsLognormalOrderStatistic) {
  const std::size_t worlds = 200'000;
  const std::vector<uint8_t> outcomes(worlds, 0);
  SampledFieldParams p = Only(20, 0.0, 0.0, 0.25);  // round(5.0) = 5 bots
  const auto t = SampleThresholds(outcomes.data(), worlds, {}, p, 2);
  for (double m : {2.0, 4.0, 8.0}) {
    const double cdf = std::pow(Phi((std::log(m) - p.bot_log_mean) / p.bot_log_sd), 5);
    EXPECT_NEAR(FractionAtMost(t, m), cdf, 0.004) << m;
  }
}

TEST(SampledThreshold, YoloPicksFollowTheBernoulliApproximation) {
  // Two contracts, both always pay: 0.02 (50x, weight 1/6) and 0.1 (10x, weight 5/6); 3 YOLOs.
  const std::size_t worlds = 200'000;
  const std::vector<uint8_t> outcomes(worlds * 2, 1);
  const auto t = SampleThresholds(outcomes.data(), worlds, {0.02, 0.1}, Only(3, 0.0, 1.0, 0.0), 3);
  const double p50 = 1.0 - std::pow(5.0 / 6.0, 3);
  const double p10 = 1.0 - std::pow(1.0 / 6.0, 3);
  const auto frac = [&](double v) {
    return static_cast<double>(std::count(t.begin(), t.end(), v)) / static_cast<double>(worlds);
  };
  EXPECT_NEAR(frac(50.0), p50, 0.004);
  EXPECT_NEAR(frac(10.0), (1.0 - p50) * p10, 0.004);
  EXPECT_NEAR(frac(1.0), (1.0 - p50) * (1.0 - p10), 0.004);
}

TEST(SampledThreshold, DeterministicAcrossThreads) {
  const std::size_t worlds = 10'001;
  std::vector<uint8_t> outcomes(worlds * 3);
  for (std::size_t i = 0; i < outcomes.size(); ++i) outcomes[i] = (i * 7) % 3 == 0;
  const std::vector<double> prices{0.05, 0.2, 0.6};
  const SampledFieldParams p;
  EXPECT_EQ(SampleThresholds(outcomes.data(), worlds, prices, p, 9, 1),
            SampleThresholds(outcomes.data(), worlds, prices, p, 9, 5));
  EXPECT_NE(SampleThresholds(outcomes.data(), worlds, prices, p, 9, 1),
            SampleThresholds(outcomes.data(), worlds, prices, p, 10, 1));
}

}  // namespace
}  // namespace susq::tourney
