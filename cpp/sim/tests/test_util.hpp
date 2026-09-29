// Small statistics helpers shared by the sim tests.
#pragma once

#include <cmath>
#include <cstddef>
#include <string>
#include <vector>

#include "susq/sim/config.hpp"
#include "susq/sim/simulator.hpp"

namespace susq::sim::testing {

inline const float* RaceErrors(const WorldSet& w, std::size_t race) {
  return &w.race_errors[race * w.n_draws];
}

inline double Mean(const float* x, std::size_t n) {
  double s = 0.0;
  for (std::size_t i = 0; i < n; ++i) s += x[i];
  return s / static_cast<double>(n);
}

inline double Cov(const float* x, const float* y, std::size_t n) {
  const double mx = Mean(x, n);
  const double my = Mean(y, n);
  double s = 0.0;
  for (std::size_t i = 0; i < n; ++i) s += (x[i] - mx) * (y[i] - my);
  return s / static_cast<double>(n - 1);
}

inline double Var(const float* x, std::size_t n) { return Cov(x, x, n); }

inline double Corr(const float* x, const float* y, std::size_t n) {
  return Cov(x, y, n) / std::sqrt(Var(x, n) * Var(y, n));
}

/// Census regions, no clusters, the requested draw count, deterministic seed.
inline SimConfig BaseConfig(std::size_t n_draws, uint64_t seed = 7) {
  SimConfig cfg = DefaultConfig();
  cfg.n_draws = n_draws;
  cfg.seed = seed;
  return cfg;
}

inline RaceSpec Race(const std::string& id, const std::string& state, double sd = 3.0) {
  RaceSpec r;
  r.id = id;
  r.state = state;
  r.sd = sd;
  return r;
}

/// Variance of the national shift implied by ShockParams (t part + miss mixture).
inline double NationalVariance(const ShockParams& s) {
  return s.national_sd * s.national_sd + s.miss_prob * s.miss_size * s.miss_size -
         std::pow(s.miss_prob * (2.0 * s.miss_prob_dem - 1.0) * s.miss_size, 2);
}

}  // namespace susq::sim::testing
