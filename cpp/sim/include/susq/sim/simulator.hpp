// Correlated race Monte Carlo: generate shared-shock worlds, calibrate race means to market
// probabilities on those worlds, evaluate outcomes and chambers, and bucket scenarios.
//
// Typical flow (what the Python calibrate/optimizer code does through pybind11):
//   SimConfig cfg = ...;                     // races with target_p_win from real-money mids
//   WorldSet worlds = Generate(cfg);         // mu-free errors, deterministic in cfg.seed
//   CalibrationReport rep = Calibrate(cfg, worlds);   // writes cfg.races[i].mu
//   Results res = Evaluate(cfg, worlds);     // outcomes, P(D), seat distributions
//   ScenarioBundles b = BuildScenarioBundles(cfg, worlds, res, {"red-state-reach"});
#pragma once

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

#include "susq/sim/config.hpp"

namespace susq::sim {

/// Draws per random-stream block. Changing it changes every draw, so it is fixed.
inline constexpr std::size_t kBlockSize = 4096;

/// The mu-independent part of every draw. Arrays are entity-major: value for entity i,
/// draw d lives at [i * n_draws + d].
struct WorldSet {
  std::size_t n_draws = 0;
  std::vector<float> national;         ///< N_d including the systematic-miss shift.
  std::vector<float> cluster_shocks;   ///< [cluster][draw]
  std::vector<float> race_errors;      ///< e_{i,d} = margin minus mu, [race][draw]
  std::vector<std::vector<uint8_t>> third_party;  ///< per race: empty, or 1 where a third party wins
  /// D-caucus seats among House districts that are not linked to a traded race; empty when no
  /// districts are configured.
  std::vector<uint16_t> house_unlinked_dem;
};

/// Per-race outcome codes in the joint outcome matrix.
enum Outcome : uint8_t { kRepWins = 0, kDemSideWins = 1, kThirdPartyWins = 2 };

struct Results {
  std::size_t n_draws = 0;
  std::vector<uint8_t> outcomes;       ///< [race][draw] of Outcome codes.
  std::vector<double> p_dem_side;      ///< P(D-side candidate wins).
  std::vector<double> p_dem_market;    ///< P(Democratic-party market resolves YES).
  std::vector<double> p_rep_market;    ///< P(Republican-party market resolves YES).
  std::vector<uint8_t> senate_dem_seats;   ///< per draw; empty when no Senate races.
  std::vector<uint16_t> house_dem_seats;   ///< per draw; empty when no districts.
  std::vector<double> senate_seat_pmf;     ///< P(D caucus seats == k).
  std::vector<double> house_seat_pmf;      ///< P(D seats == k).
  double p_senate_dem_control = 0.0;       ///< NaN when the Senate is not modeled.
  double p_house_dem_control = 0.0;        ///< NaN when the House is not modeled.
  std::vector<uint8_t> senate_dem_control; ///< per draw 0/1; empty when not modeled.
  std::vector<uint8_t> house_dem_control;  ///< per draw 0/1; empty when not modeled.
};

struct CalibrationEntry {
  std::string race_id;
  double target = 0.0;     ///< requested P(D-side wins)
  double achieved = 0.0;   ///< realized on the world set
  double mu = 0.0;         ///< solved mean margin
  bool feasible = true;    ///< false when target > 1 - realized third-party share
};

struct CalibrationReport {
  std::vector<CalibrationEntry> entries;  ///< one per race that had a target
  double max_abs_error = 0.0;
};

struct ScenarioBucket {
  int national_decile = 0;       ///< 0 = most R-leaning tenth of N ... 9 = most D-leaning.
  uint32_t cluster_signs = 0;    ///< bit c set when the c-th requested cluster shock is > 0 (pro-D).
  std::size_t n_draws = 0;
  double probability = 0.0;
  double mean_national = 0.0;
  std::vector<double> p_dem_market;  ///< per race, conditional on the bucket
  std::vector<double> p_rep_market;
  double p_senate_dem_control = 0.0; ///< NaN when not modeled
  double p_house_dem_control = 0.0;
};

struct ScenarioBundles {
  std::vector<std::string> clusters;     ///< cluster names defining the sign bits, in bit order
  std::vector<double> decile_edges;      ///< 9 interior cut points of N
  std::vector<ScenarioBucket> buckets;   ///< 10 * 2^k buckets, index = decile * 2^k + signs
  std::vector<uint32_t> draw_bucket;     ///< bucket index of each draw
};

/// Draws the shared shocks, race errors, third-party events and unlinked House districts.
WorldSet Generate(const SimConfig& config);

/// Solves mu for every race with a target so that the realized P(D-side wins) on `worlds`
/// matches it (to within 1/(2*n_draws)). Common random numbers: `worlds` is reused as is.
CalibrationReport Calibrate(SimConfig& config, const WorldSet& worlds);

/// Applies each race's mu to the world set and aggregates chambers.
Results Evaluate(const SimConfig& config, const WorldSet& worlds);

/// Buckets draws by national-shift decile x signs of the named cluster shocks (at most 16).
ScenarioBundles BuildScenarioBundles(const SimConfig& config, const WorldSet& worlds,
                                     const Results& results,
                                     const std::vector<std::string>& cluster_names);

}  // namespace susq::sim
