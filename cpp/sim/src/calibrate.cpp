#include <algorithm>
#include <cmath>
#include <stdexcept>
#include <vector>

#include "layout.hpp"
#include "susq/sim/parallel.hpp"
#include "susq/sim/simulator.hpp"

namespace susq::sim {
namespace {

// Finds the threshold q such that exactly `m` of `errors` exceed it, placing q halfway between
// the neighbouring order statistics. This is the exact root of the (step) empirical survival
// function that bisection on mu would converge to, found by selection in O(K).
double ThresholdWithCount(std::vector<float>& errors, std::size_t m) {
  const std::size_t k = errors.size();
  if (k == 0) return 0.0;
  if (m == 0) return static_cast<double>(*std::max_element(errors.begin(), errors.end())) + 1.0;
  if (m >= k) return static_cast<double>(*std::min_element(errors.begin(), errors.end())) - 1.0;
  const auto split = errors.begin() + static_cast<std::ptrdiff_t>(k - m);
  std::nth_element(errors.begin(), split, errors.end());
  const double upper = *split;  // smallest error that must exceed q
  const double lower = *std::max_element(errors.begin(), split);  // largest that must not
  return 0.5 * (lower + upper);
}

}  // namespace

CalibrationReport Calibrate(SimConfig& config, const WorldSet& worlds) {
  (void)detail::BuildLayout(config);
  const std::size_t n = worlds.n_draws;
  if (n != config.n_draws || worlds.race_errors.size() != config.races.size() * n) {
    throw std::invalid_argument("Calibrate: world set does not match the config (regenerate)");
  }
  std::vector<std::size_t> targets;
  for (std::size_t i = 0; i < config.races.size(); ++i) {
    if (config.races[i].target_p_win) targets.push_back(i);
  }
  CalibrationReport report;
  report.entries.resize(targets.size());

  ParallelFor(targets.size(), config.threads, [&](std::size_t t) {
    const std::size_t i = targets[t];
    RaceSpec& race = config.races[i];
    const double target = *race.target_p_win;
    const float* e = &worlds.race_errors[i * n];
    const auto& third = worlds.third_party[i];

    std::vector<float> eligible;
    eligible.reserve(n);
    for (std::size_t d = 0; d < n; ++d) {
      if (third.empty() || third[d] == 0) eligible.push_back(e[d]);
    }
    const auto wanted = static_cast<std::size_t>(std::llround(target * static_cast<double>(n)));
    const bool feasible = wanted <= eligible.size();
    const double q = ThresholdWithCount(eligible, std::min(wanted, eligible.size()));
    race.mu = -q;

    std::size_t wins = 0;
    for (std::size_t d = 0; d < n; ++d) {
      if ((third.empty() || third[d] == 0) && race.mu + static_cast<double>(e[d]) > 0.0) ++wins;
    }
    CalibrationEntry& entry = report.entries[t];
    entry.race_id = race.id;
    entry.target = target;
    entry.achieved = static_cast<double>(wins) / static_cast<double>(n);
    entry.mu = race.mu;
    entry.feasible = feasible;
  });
  for (const auto& entry : report.entries) {
    report.max_abs_error = std::max(report.max_abs_error, std::abs(entry.achieved - entry.target));
  }
  return report;
}

}  // namespace susq::sim
