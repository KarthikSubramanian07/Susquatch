#include <algorithm>
#include <limits>
#include <stdexcept>
#include <vector>

#include "susq/sim/simulator.hpp"

namespace susq::sim {
namespace {

std::vector<double> DecileEdges(const std::vector<float>& national) {
  std::vector<float> sorted = national;
  std::vector<double> edges;
  for (int k = 1; k <= 9; ++k) {
    const auto idx = static_cast<std::size_t>(static_cast<double>(k) / 10.0 *
                                              static_cast<double>(sorted.size()));
    const auto it = sorted.begin() + static_cast<std::ptrdiff_t>(std::min(idx, sorted.size() - 1));
    std::nth_element(sorted.begin(), it, sorted.end());
    edges.push_back(*it);
  }
  return edges;
}

}  // namespace

ScenarioBundles BuildScenarioBundles(const SimConfig& config, const WorldSet& worlds,
                                     const Results& results,
                                     const std::vector<std::string>& cluster_names) {
  const std::size_t n = worlds.n_draws;
  const std::size_t n_races = config.races.size();
  if (results.n_draws != n || results.outcomes.size() != n_races * n) {
    throw std::invalid_argument("BuildScenarioBundles: results do not match the world set");
  }
  if (cluster_names.size() > 16) throw std::invalid_argument("BuildScenarioBundles: at most 16 clusters");
  std::vector<std::size_t> cluster_idx;
  for (const auto& name : cluster_names) {
    auto it = std::find_if(config.clusters.begin(), config.clusters.end(),
                           [&](const ClusterSpec& c) { return c.name == name; });
    if (it == config.clusters.end()) throw std::invalid_argument("unknown cluster '" + name + "'");
    cluster_idx.push_back(static_cast<std::size_t>(it - config.clusters.begin()));
  }

  ScenarioBundles out;
  out.clusters = cluster_names;
  out.decile_edges = DecileEdges(worlds.national);
  const uint32_t n_signs = 1U << cluster_idx.size();
  const std::size_t n_buckets = 10 * static_cast<std::size_t>(n_signs);
  out.draw_bucket.resize(n);
  for (std::size_t d = 0; d < n; ++d) {
    const double x = worlds.national[d];
    const auto decile = static_cast<uint32_t>(
        std::upper_bound(out.decile_edges.begin(), out.decile_edges.end(), x) - out.decile_edges.begin());
    uint32_t signs = 0;
    for (std::size_t c = 0; c < cluster_idx.size(); ++c) {
      if (worlds.cluster_shocks[cluster_idx[c] * n + d] > 0.0F) signs |= 1U << c;
    }
    out.draw_bucket[d] = decile * n_signs + signs;
  }

  std::vector<std::size_t> count(n_buckets, 0);
  std::vector<double> sum_national(n_buckets, 0.0);
  std::vector<std::size_t> senate(n_buckets, 0);
  std::vector<std::size_t> house(n_buckets, 0);
  std::vector<std::size_t> dem(n_buckets * n_races, 0);
  std::vector<std::size_t> rep(n_buckets * n_races, 0);
  for (std::size_t d = 0; d < n; ++d) {
    const std::size_t b = out.draw_bucket[d];
    ++count[b];
    sum_national[b] += worlds.national[d];
    if (!results.senate_dem_control.empty()) senate[b] += results.senate_dem_control[d];
    if (!results.house_dem_control.empty()) house[b] += results.house_dem_control[d];
  }
  for (std::size_t i = 0; i < n_races; ++i) {
    const uint8_t* o = &results.outcomes[i * n];
    const bool independent = config.races[i].dem_side_is_independent;
    for (std::size_t d = 0; d < n; ++d) {
      const std::size_t b = out.draw_bucket[d];
      if (o[d] == kDemSideWins && !independent) ++dem[b * n_races + i];
      if (o[d] == kRepWins) ++rep[b * n_races + i];
    }
  }

  const double nan = std::numeric_limits<double>::quiet_NaN();
  out.buckets.resize(n_buckets);
  for (std::size_t b = 0; b < n_buckets; ++b) {
    ScenarioBucket& s = out.buckets[b];
    s.national_decile = static_cast<int>(b / n_signs);
    s.cluster_signs = static_cast<uint32_t>(b % n_signs);
    s.n_draws = count[b];
    s.probability = static_cast<double>(count[b]) / static_cast<double>(n);
    const double denom = count[b] > 0 ? static_cast<double>(count[b]) : nan;
    s.mean_national = sum_national[b] / denom;
    s.p_dem_market.resize(n_races);
    s.p_rep_market.resize(n_races);
    for (std::size_t i = 0; i < n_races; ++i) {
      s.p_dem_market[i] = static_cast<double>(dem[b * n_races + i]) / denom;
      s.p_rep_market[i] = static_cast<double>(rep[b * n_races + i]) / denom;
    }
    s.p_senate_dem_control = results.senate_dem_control.empty() ? nan : static_cast<double>(senate[b]) / denom;
    s.p_house_dem_control = results.house_dem_control.empty() ? nan : static_cast<double>(house[b]) / denom;
  }
  return out;
}

}  // namespace susq::sim
