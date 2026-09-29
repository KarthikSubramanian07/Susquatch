#include <gtest/gtest.h>

#include <cmath>
#include <vector>

#include "susq/sim/rng.hpp"

namespace susq::sim {
namespace {

constexpr int kN = 1'000'000;

struct Moments {
  double mean = 0.0;
  double var = 0.0;
  double excess_kurtosis = 0.0;
};

template <class Gen>
Moments Measure(Gen&& gen) {
  std::vector<double> x(kN);
  double s = 0.0;
  for (auto& v : x) s += (v = gen());
  Moments m;
  m.mean = s / kN;
  double m2 = 0.0;
  double m4 = 0.0;
  for (double v : x) {
    const double d = (v - m.mean) * (v - m.mean);
    m2 += d;
    m4 += d * d;
  }
  m.var = m2 / kN;
  m.excess_kurtosis = (m4 / kN) / (m.var * m.var) - 3.0;
  return m;
}

TEST(Rng, StreamsAreReproducibleAndDistinct) {
  Xoshiro256pp a(1, HashKey("race:A"), 0);
  Xoshiro256pp b(1, HashKey("race:A"), 0);
  Xoshiro256pp c(1, HashKey("race:B"), 0);
  Xoshiro256pp d(1, HashKey("race:A"), 1);
  for (int i = 0; i < 100; ++i) {
    const uint64_t va = a.Next();
    EXPECT_EQ(va, b.Next());
    EXPECT_NE(va, c.Next());
    EXPECT_NE(va, d.Next());
  }
}

TEST(Rng, UniformIsOpenUnitInterval) {
  Xoshiro256pp r(3, 4, 5);
  double lo = 1.0;
  double hi = 0.0;
  double sum = 0.0;
  for (int i = 0; i < kN; ++i) {
    const double u = r.UniformOpen();
    lo = std::min(lo, u);
    hi = std::max(hi, u);
    sum += u;
  }
  EXPECT_GT(lo, 0.0);
  EXPECT_LT(hi, 1.0);
  EXPECT_NEAR(sum / kN, 0.5, 0.002);
}

TEST(Rng, NormalMoments) {
  Sampler s(Xoshiro256pp(11, 0, 0));
  const Moments m = Measure([&] { return s.Normal(); });
  EXPECT_NEAR(m.mean, 0.0, 0.005);
  EXPECT_NEAR(m.var, 1.0, 0.01);
  EXPECT_NEAR(m.excess_kurtosis, 0.0, 0.05);
}

TEST(Rng, NormalTailsMatchTheGaussian) {
  // Exercises the ziggurat wedges and the tail beyond R = 3.44.
  Sampler s(Xoshiro256pp(14, 0, 0));
  const int n = 4'000'000;
  int beyond1 = 0;
  int beyond2 = 0;
  int beyond3 = 0;
  int beyond_r = 0;
  for (int i = 0; i < n; ++i) {
    const double z = std::abs(s.Normal());
    beyond1 += z > 1.0;
    beyond2 += z > 2.0;
    beyond3 += z > 3.0;
    beyond_r += z > 3.5;
  }
  auto frac = [&](int k) { return static_cast<double>(k) / n; };
  EXPECT_NEAR(frac(beyond1), std::erfc(1.0 / std::sqrt(2.0)), 0.0008);
  EXPECT_NEAR(frac(beyond2), std::erfc(2.0 / std::sqrt(2.0)), 0.0004);
  EXPECT_NEAR(frac(beyond3), std::erfc(3.0 / std::sqrt(2.0)), 0.0001);
  EXPECT_NEAR(frac(beyond_r), std::erfc(3.5 / std::sqrt(2.0)), 0.00004);
}

TEST(Rng, GammaMomentsIncludingSmallShape) {
  for (double shape : {0.5, 2.5, 7.0}) {
    Sampler s(Xoshiro256pp(12, 0, 0));
    const Moments m = Measure([&] { return s.Gamma(shape); });
    EXPECT_NEAR(m.mean, shape, 0.01 * shape + 0.005) << "shape " << shape;
    EXPECT_NEAR(m.var, shape, 0.03 * shape) << "shape " << shape;
  }
}

TEST(Rng, StudentTVarianceAndTails) {
  // Var = nu/(nu-2); excess kurtosis = 6/(nu-4).
  Sampler s(Xoshiro256pp(13, 0, 0));
  const Moments m10 = Measure([&] { return s.StudentT(10.0); });
  EXPECT_NEAR(m10.mean, 0.0, 0.01);
  EXPECT_NEAR(m10.var, 10.0 / 8.0, 0.02);
  EXPECT_NEAR(m10.excess_kurtosis, 1.0, 0.25);

  const Moments m5 = Measure([&] { return s.StudentT(5.0); });
  EXPECT_NEAR(m5.var, 5.0 / 3.0, 0.06);
  EXPECT_GT(m5.excess_kurtosis, 2.5);  // heavy tails (theoretical 6; sample estimate is noisy)
}

}  // namespace
}  // namespace susq::sim
