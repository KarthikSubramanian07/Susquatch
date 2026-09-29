// Inputs to the correlated race Monte Carlo (SPEC §5, docs/MODELS.md §1).
#pragma once

#include <cstddef>
#include <cstdint>
#include <map>
#include <optional>
#include <string>
#include <vector>

namespace susq::sim {

/// Shared-shock parameters, in points of D-R margin.
struct ShockParams {
  double national_sd = 2.75;    ///< SD of the Student-t national shift N (SPEC §5: 2.5-3.0).
  double national_nu = 5.0;     ///< Degrees of freedom of N (SPEC §5: 4-6); must be > 2.
  double region_sd = 1.5;       ///< SD of each region shock R_region.
  double state_sd = 2.5;        ///< SD of each state shock S_state (shared by every race in the state).
  double miss_prob = 0.1;       ///< Probability of a systematic polling miss in a draw.
  double miss_size = 3.5;       ///< Size of the extra national shift when a miss happens.
  double miss_prob_dem = 0.5;   ///< P(the miss favours D | miss); 0.5 = sign-symmetric.
};

/// A named correlated cluster with its own N(0, sd^2) shock per draw.
struct ClusterSpec {
  std::string name;  ///< e.g. "red-state-reach", "midwest-industrial".
  double sd = 2.0;
};

/// Membership of a race or district in a cluster; the shock enters as loading * C_cluster.
struct ClusterLoading {
  std::string cluster;
  double loading = 1.0;
};

enum class Chamber : uint8_t { kNone = 0, kSenate = 1, kHouse = 2, kGovernor = 3 };

/// One traded race. Margin in draw d is mu + e_d with
/// e_d = beta*N_d + R_region(d) + S_state(d) + sum_c loading_c*C_c(d) + eps_d.
struct RaceSpec {
  std::string id;                        ///< Unique, e.g. "2026-SEN-OH-S".
  std::string state;                     ///< USPS code; must appear in the region map.
  Chamber chamber = Chamber::kNone;
  double mu = 0.0;                       ///< Mean D-side minus R margin (pts); set by calibration.
  double sd = 3.0;                       ///< Race-specific error SD eps (SPEC §5: 2.5-3.5).
  double national_beta = 1.0;            ///< Loading on the national shift.
  std::vector<ClusterLoading> clusters;
  /// P(a candidate outside the modeled margin wins), drawn independently of the shocks.
  /// Both party markets resolve NO in those draws.
  double p_third_party = 0.0;
  /// The D-side candidate of the margin is an independent (e.g. NE Senate, Osborn):
  /// the Democratic market resolves NO even when that side wins.
  bool dem_side_is_independent = false;
  /// Whether a D-side win adds to the Democratic caucus in chamber counts.
  bool dem_side_caucuses_dem = true;
  /// Calibration target P(D-side candidate wins), e.g. a real-money mid; unset = keep mu.
  std::optional<double> target_p_win;
};

/// A House district not traded individually (or linked to a traded race via `race_id`).
/// Margin = generic_ballot + lv_adjustment + N + H + lean + incumbency + R + S + clusters + noise.
struct HouseDistrictSpec {
  std::string id;          ///< e.g. "CA-13".
  std::string state;
  double lean = 0.0;       ///< District lean relative to the national House vote (pts, + = D).
  double incumbency = 0.0; ///< Incumbency/candidate effect (pts, + = D).
  std::vector<ClusterLoading> clusters;
  /// If set (or if a race with the same id exists), the district's outcome is that race's
  /// calibrated outcome instead of an independent draw.
  std::string race_id;
};

struct SenateSpec {
  int dem_seats_not_up = 0;        ///< D-caucus seats not on the ballot.
  int control_threshold = 51;      ///< Seats needed for outright control.
  bool vp_breaks_ties_dem = false; ///< 50 seats + a D vice president = D control.
};

struct HouseSpec {
  double generic_ballot = 7.7;         ///< D-R generic ballot mean (RESEARCH §5: D+7.4 to D+8.1).
  double lv_adjustment = 0.0;          ///< Likely-voter adjustment added to the generic ballot.
  double national_extra_sd = 0.0;      ///< Extra House-vote noise H beyond the shared N.
  double district_sd = 5.0;            ///< District noise SD (total idiosyncratic ~5.8 with R+S).
  int control_threshold = 218;
};

/// Full simulator configuration.
struct SimConfig {
  ShockParams shocks;
  std::vector<std::string> regions;                  ///< Region names.
  std::map<std::string, std::string> state_region;   ///< USPS code -> region name.
  std::vector<ClusterSpec> clusters;
  std::vector<RaceSpec> races;
  std::vector<HouseDistrictSpec> districts;          ///< Empty = no House aggregation.
  SenateSpec senate;
  HouseSpec house;
  uint64_t seed = 20261103;
  std::size_t n_draws = 100000;
  unsigned threads = 0;                              ///< 0 = std::thread::hardware_concurrency().
};

/// Census regions {Northeast, Midwest, South, West} for the 50 states + DC.
std::map<std::string, std::string> DefaultStateRegions();

/// A config with the census regions pre-filled and SPEC §5 shock defaults.
SimConfig DefaultConfig();

/// Throws std::invalid_argument describing the first problem found.
void Validate(const SimConfig& config);

}  // namespace susq::sim
