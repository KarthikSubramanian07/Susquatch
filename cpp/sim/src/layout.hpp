// Index form of a SimConfig: names resolved once so hot loops work on integers.
#pragma once

#include <string>
#include <utility>
#include <vector>

#include "susq/sim/config.hpp"

namespace susq::sim::detail {

using Loadings = std::vector<std::pair<int, double>>;  // (cluster index, loading)

struct Layout {
  std::vector<std::string> states;  ///< every state referenced by a race or district, sorted
  std::vector<int> state_region;    ///< region index per entry of `states`
  std::vector<int> race_state;
  std::vector<Loadings> race_clusters;
  std::vector<int> district_state;
  std::vector<Loadings> district_clusters;
  std::vector<int> district_race;   ///< linked race index, or -1
  std::vector<int> senate_races;    ///< indices of Senate races
};

/// Resolves and validates; throws std::invalid_argument on the first problem.
Layout BuildLayout(const SimConfig& config);

}  // namespace susq::sim::detail
