#include <algorithm>
#include <cmath>
#include <set>
#include <stdexcept>
#include <unordered_map>

#include "layout.hpp"
#include "susq/sim/config.hpp"

namespace susq::sim {
namespace {

[[noreturn]] void Fail(const std::string& what) { throw std::invalid_argument("sim config: " + what); }

void RequireFiniteNonNegative(double v, const std::string& name) {
  if (!std::isfinite(v) || v < 0.0) Fail(name + " must be finite and >= 0");
}

detail::Loadings ResolveLoadings(const std::vector<ClusterLoading>& in,
                                 const std::unordered_map<std::string, int>& cluster_index,
                                 const std::string& owner) {
  detail::Loadings out;
  for (const auto& l : in) {
    auto it = cluster_index.find(l.cluster);
    if (it == cluster_index.end()) Fail(owner + " references unknown cluster '" + l.cluster + "'");
    if (!std::isfinite(l.loading)) Fail(owner + " has a non-finite cluster loading");
    out.emplace_back(it->second, l.loading);
  }
  return out;
}

}  // namespace

std::map<std::string, std::string> DefaultStateRegions() {
  static const char* const kNortheast[] = {"CT", "ME", "MA", "NH", "RI", "VT", "NJ", "NY", "PA"};
  static const char* const kMidwest[] = {"IL", "IN", "MI", "OH", "WI", "IA",
                                         "KS", "MN", "MO", "NE", "ND", "SD"};
  static const char* const kSouth[] = {"DE", "DC", "FL", "GA", "MD", "NC", "SC", "VA", "WV",
                                       "AL", "KY", "MS", "TN", "AR", "LA", "OK", "TX"};
  static const char* const kWest[] = {"AZ", "CO", "ID", "MT", "NV", "NM", "UT",
                                      "WY", "AK", "CA", "HI", "OR", "WA"};
  std::map<std::string, std::string> m;
  for (const char* s : kNortheast) m[s] = "Northeast";
  for (const char* s : kMidwest) m[s] = "Midwest";
  for (const char* s : kSouth) m[s] = "South";
  for (const char* s : kWest) m[s] = "West";
  return m;
}

SimConfig DefaultConfig() {
  SimConfig c;
  c.regions = {"Northeast", "Midwest", "South", "West"};
  c.state_region = DefaultStateRegions();
  return c;
}

void Validate(const SimConfig& config) { (void)detail::BuildLayout(config); }

namespace detail {

Layout BuildLayout(const SimConfig& config) {
  const ShockParams& s = config.shocks;
  RequireFiniteNonNegative(s.national_sd, "national_sd");
  RequireFiniteNonNegative(s.region_sd, "region_sd");
  RequireFiniteNonNegative(s.state_sd, "state_sd");
  RequireFiniteNonNegative(s.miss_size, "miss_size");
  if (!(s.national_nu > 2.0) || !std::isfinite(s.national_nu)) Fail("national_nu must be > 2");
  if (!(s.miss_prob >= 0.0 && s.miss_prob <= 1.0)) Fail("miss_prob must be in [0,1]");
  if (!(s.miss_prob_dem >= 0.0 && s.miss_prob_dem <= 1.0)) Fail("miss_prob_dem must be in [0,1]");
  if (config.n_draws == 0) Fail("n_draws must be > 0");
  RequireFiniteNonNegative(config.house.district_sd, "house.district_sd");
  RequireFiniteNonNegative(config.house.national_extra_sd, "house.national_extra_sd");
  if (!std::isfinite(config.house.generic_ballot) || !std::isfinite(config.house.lv_adjustment)) {
    Fail("house generic_ballot/lv_adjustment must be finite");
  }

  std::unordered_map<std::string, int> region_index;
  for (std::size_t i = 0; i < config.regions.size(); ++i) {
    if (!region_index.emplace(config.regions[i], static_cast<int>(i)).second) {
      Fail("duplicate region '" + config.regions[i] + "'");
    }
  }
  std::unordered_map<std::string, int> cluster_index;
  for (std::size_t i = 0; i < config.clusters.size(); ++i) {
    const auto& c = config.clusters[i];
    RequireFiniteNonNegative(c.sd, "cluster '" + c.name + "' sd");
    if (!cluster_index.emplace(c.name, static_cast<int>(i)).second) {
      Fail("duplicate cluster '" + c.name + "'");
    }
  }

  std::set<std::string> state_set;
  for (const auto& r : config.races) {
    if (!r.state.empty()) state_set.insert(r.state);
  }
  for (const auto& d : config.districts) {
    if (!d.state.empty()) state_set.insert(d.state);
  }

  Layout layout;
  layout.states.assign(state_set.begin(), state_set.end());
  std::unordered_map<std::string, int> state_index;
  for (std::size_t i = 0; i < layout.states.size(); ++i) {
    const std::string& st = layout.states[i];
    state_index[st] = static_cast<int>(i);
    auto it = config.state_region.find(st);
    if (it == config.state_region.end()) Fail("state '" + st + "' has no region");
    auto rit = region_index.find(it->second);
    if (rit == region_index.end()) Fail("state '" + st + "' maps to unknown region '" + it->second + "'");
    layout.state_region.push_back(rit->second);
  }

  std::unordered_map<std::string, int> race_index;
  for (std::size_t i = 0; i < config.races.size(); ++i) {
    const RaceSpec& r = config.races[i];
    const std::string who = "race '" + r.id + "'";
    if (r.id.empty()) Fail("race with empty id");
    if (!race_index.emplace(r.id, static_cast<int>(i)).second) Fail("duplicate " + who);
    if (!std::isfinite(r.mu) || !std::isfinite(r.national_beta)) Fail(who + " mu/beta not finite");
    RequireFiniteNonNegative(r.sd, who + " sd");
    if (!(r.p_third_party >= 0.0 && r.p_third_party < 1.0)) Fail(who + " p_third_party not in [0,1)");
    if (r.target_p_win && !(*r.target_p_win > 0.0 && *r.target_p_win < 1.0)) {
      Fail(who + " target_p_win must be in (0,1)");
    }
    layout.race_state.push_back(r.state.empty() ? -1 : state_index.at(r.state));
    layout.race_clusters.push_back(ResolveLoadings(r.clusters, cluster_index, who));
    if (r.chamber == Chamber::kSenate) layout.senate_races.push_back(static_cast<int>(i));
  }

  std::set<std::string> district_ids;
  for (const auto& d : config.districts) {
    const std::string who = "district '" + d.id + "'";
    if (d.id.empty()) Fail("district with empty id");
    if (!district_ids.insert(d.id).second) Fail("duplicate " + who);
    if (!std::isfinite(d.lean) || !std::isfinite(d.incumbency)) Fail(who + " lean/incumbency not finite");
    layout.district_state.push_back(d.state.empty() ? -1 : state_index.at(d.state));
    layout.district_clusters.push_back(ResolveLoadings(d.clusters, cluster_index, who));
    const std::string& link = d.race_id.empty() ? d.id : d.race_id;
    auto it = race_index.find(link);
    if (it == race_index.end() && !d.race_id.empty()) Fail(who + " links to unknown race '" + d.race_id + "'");
    layout.district_race.push_back(it == race_index.end() ? -1 : it->second);
  }
  if (config.senate.control_threshold < 1) Fail("senate.control_threshold must be >= 1");
  if (config.house.control_threshold < 1) Fail("house.control_threshold must be >= 1");
  if (config.senate.dem_seats_not_up < 0) Fail("senate.dem_seats_not_up must be >= 0");
  return layout;
}

}  // namespace detail
}  // namespace susq::sim
