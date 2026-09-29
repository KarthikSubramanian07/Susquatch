#include "susq/tourney/sampled.hpp"

#include <algorithm>
#include <cmath>
#include <limits>
#include <numbers>
#include <stdexcept>

#include "susq/sim/parallel.hpp"
#include "susq/sim/rng.hpp"

namespace susq::tourney {
namespace {

constexpr std::size_t kWorldBlock = 4096;

uint32_t TypeCount(uint32_t n, double frac) {
  // Python's round(): half to even, which std::nearbyint gives in the default rounding mode.
  return static_cast<uint32_t>(std::max(0.0, std::nearbyint(static_cast<double>(n) * frac)));
}

}  // namespace

double NormalQuantile(double p) {
  if (!(p > 0.0 && p < 1.0)) {
    if (p == 0.0) return -std::numeric_limits<double>::infinity();
    if (p == 1.0) return std::numeric_limits<double>::infinity();
    return std::numeric_limits<double>::quiet_NaN();
  }
  static constexpr double a[] = {-3.969683028665376e+01, 2.209460984245205e+02, -2.759285104469687e+02,
                                 1.383577518672690e+02,  -3.066479806614716e+01, 2.506628277459239e+00};
  static constexpr double b[] = {-5.447609879822406e+01, 1.615858368580409e+02, -1.556989798598866e+02,
                                 6.680131188771972e+01, -1.328068155288572e+01};
  static constexpr double c[] = {-7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e+00,
                                 -2.549732539343734e+00, 4.374664141464968e+00,  2.938163982698783e+00};
  static constexpr double d[] = {7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e+00,
                                 3.754408661907416e+00};
  constexpr double p_low = 0.02425;
  double x = 0.0;
  if (p < p_low) {
    const double q = std::sqrt(-2.0 * std::log(p));
    x = (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]) /
        ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0);
  } else if (p <= 1.0 - p_low) {
    const double q = p - 0.5;
    const double r = q * q;
    x = (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5]) * q /
        (((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1.0);
  } else {
    const double q = std::sqrt(-2.0 * std::log1p(-p));
    x = -(((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]) /
        ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0);
  }
  // One Halley step on Phi(x) - p.
  const double e = 0.5 * std::erfc(-x / std::numbers::sqrt2) - p;
  const double u = e * std::sqrt(2.0 * std::numbers::pi) * std::exp(0.5 * x * x);
  return x - u / (1.0 + 0.5 * x * u);
}

std::vector<double> SampleThresholds(const uint8_t* outcomes, std::size_t n_worlds, const std::vector<double>& prices,
                                     const SampledFieldParams& p, uint64_t seed, unsigned threads) {
  for (double f : {p.frac_kelly, p.frac_yolo, p.frac_bot}) {
    if (!(f >= 0.0 && f <= 1.0)) throw std::invalid_argument("tourney: type fractions must be in [0,1]");
  }
  if (!(p.kelly_lo <= p.kelly_hi) || !(p.bot_log_sd >= 0.0)) {
    throw std::invalid_argument("tourney: need kelly_lo <= kelly_hi and bot_log_sd >= 0");
  }
  const std::size_t n_contracts = prices.size();
  const uint32_t n_kelly = TypeCount(p.n_players, p.frac_kelly);
  const uint32_t n_bot = TypeCount(p.n_players, p.frac_bot);
  const uint32_t n_yolo = TypeCount(p.n_players, p.frac_yolo);

  std::vector<double> clipped(n_contracts);
  std::vector<double> pick_prob(n_contracts, 0.0);
  double total = 0.0;
  for (std::size_t j = 0; j < n_contracts; ++j) {
    clipped[j] = std::clamp(prices[j], 1e-3, 1.0);
    total += std::pow(clipped[j], p.yolo_price_power);
  }
  for (std::size_t j = 0; j < n_contracts && total > 0.0; ++j) {
    const double w = std::pow(clipped[j], p.yolo_price_power) / total;
    pick_prob[j] = -std::expm1(static_cast<double>(n_yolo) * std::log1p(-std::min(w, 1.0)));
  }

  std::vector<double> t(n_worlds, 1.0);
  const std::size_t n_blocks = (n_worlds + kWorldBlock - 1) / kWorldBlock;
  susq::sim::ParallelFor(n_blocks, threads, [&](std::size_t b) {
    susq::sim::Sampler s(susq::sim::Xoshiro256pp(seed, susq::sim::HashKey("threshold"), b));
    const std::size_t end = std::min(n_worlds, (b + 1) * kWorldBlock);
    for (std::size_t w = b * kWorldBlock; w < end; ++w) {
      double best = 1.0;
      if (n_kelly > 0) {
        const double u = std::pow(s.Uniform(), 1.0 / n_kelly);
        best = std::max(best, p.kelly_lo + (p.kelly_hi - p.kelly_lo) * u);
      }
      if (n_bot > 0) {
        const double u = std::pow(s.Uniform(), 1.0 / n_bot);
        best = std::max(best, std::exp(p.bot_log_mean + p.bot_log_sd * NormalQuantile(u)));
      }
      if (n_yolo > 0) {
        const uint8_t* row = outcomes + w * n_contracts;
        for (std::size_t j = 0; j < n_contracts; ++j) {
          const bool picked = s.Uniform() < pick_prob[j];
          if (picked && row[j] != 0) best = std::max(best, 1.0 / clipped[j]);
        }
      }
      t[w] = best;
    }
  });
  return t;
}

}  // namespace susq::tourney
