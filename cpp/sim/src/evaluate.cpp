#include <algorithm>
#include <limits>
#include <stdexcept>
#include <vector>

#include "layout.hpp"
#include "susq/sim/parallel.hpp"
#include "susq/sim/simulator.hpp"

namespace susq::sim {
namespace {

template <class T>
std::vector<double> Pmf(const std::vector<T>& counts, std::size_t max_value) {
  std::vector<double> pmf(max_value + 1, 0.0);
  for (T c : counts) pmf[static_cast<std::size_t>(c)] += 1.0;
  for (double& p : pmf) p /= static_cast<double>(counts.size());
  return pmf;
}

double Mean(const std::vector<uint8_t>& flags) {
  std::size_t s = 0;
  for (uint8_t f : flags) s += f;
  return static_cast<double>(s) / static_cast<double>(flags.size());
}

}  // namespace

Results Evaluate(const SimConfig& config, const WorldSet& worlds) {
  const detail::Layout layout = detail::BuildLayout(config);
  const std::size_t n = worlds.n_draws;
  const std::size_t n_races = config.races.size();
  if (n != config.n_draws || worlds.race_errors.size() != n_races * n) {
    throw std::invalid_argument("Evaluate: world set does not match the config (regenerate)");
  }
  Results res;
  res.n_draws = n;
  res.outcomes.resize(n_races * n);
  const bool senate = !layout.senate_races.empty();
  const bool house = !config.districts.empty();
  if (senate) res.senate_dem_seats.resize(n);
  if (house) res.house_dem_seats.resize(n);

  const std::size_t n_blocks = (n + kBlockSize - 1) / kBlockSize;
  ParallelFor(n_blocks, config.threads, [&](std::size_t b) {
    const std::size_t begin = b * kBlockSize;
    const std::size_t end = std::min(n, begin + kBlockSize);
    for (std::size_t i = 0; i < n_races; ++i) {
      const double mu = config.races[i].mu;
      const float* e = &worlds.race_errors[i * n];
      const auto& third = worlds.third_party[i];
      uint8_t* out = &res.outcomes[i * n];
      for (std::size_t d = begin; d < end; ++d) {
        if (!third.empty() && third[d] != 0) {
          out[d] = kThirdPartyWins;
        } else {
          out[d] = mu + static_cast<double>(e[d]) > 0.0 ? kDemSideWins : kRepWins;
        }
      }
    }
    auto dem_caucus_win = [&](std::size_t race, std::size_t d) {
      return res.outcomes[race * n + d] == kDemSideWins && config.races[race].dem_side_caucuses_dem;
    };
    for (std::size_t d = begin; d < end && senate; ++d) {
      int seats = config.senate.dem_seats_not_up;
      for (int r : layout.senate_races) seats += dem_caucus_win(static_cast<std::size_t>(r), d) ? 1 : 0;
      res.senate_dem_seats[d] = static_cast<uint8_t>(std::min(seats, 255));
    }
    for (std::size_t d = begin; d < end && house; ++d) {
      int seats = worlds.house_unlinked_dem[d];
      for (int r : layout.district_race) {
        if (r >= 0) seats += dem_caucus_win(static_cast<std::size_t>(r), d) ? 1 : 0;
      }
      res.house_dem_seats[d] = static_cast<uint16_t>(seats);
    }
  });

  res.p_dem_side.resize(n_races);
  res.p_dem_market.resize(n_races);
  res.p_rep_market.resize(n_races);
  for (std::size_t i = 0; i < n_races; ++i) {
    std::size_t dem = 0;
    std::size_t rep = 0;
    for (std::size_t d = 0; d < n; ++d) {
      const uint8_t o = res.outcomes[i * n + d];
      dem += o == kDemSideWins ? 1 : 0;
      rep += o == kRepWins ? 1 : 0;
    }
    res.p_dem_side[i] = static_cast<double>(dem) / static_cast<double>(n);
    res.p_dem_market[i] = config.races[i].dem_side_is_independent ? 0.0 : res.p_dem_side[i];
    res.p_rep_market[i] = static_cast<double>(rep) / static_cast<double>(n);
  }

  const double nan = std::numeric_limits<double>::quiet_NaN();
  res.p_senate_dem_control = nan;
  res.p_house_dem_control = nan;
  if (senate) {
    const int thr = config.senate.control_threshold;
    res.senate_dem_control.resize(n);
    for (std::size_t d = 0; d < n; ++d) {
      const int s = res.senate_dem_seats[d];
      res.senate_dem_control[d] = (s >= thr || (config.senate.vp_breaks_ties_dem && s == thr - 1)) ? 1 : 0;
    }
    res.senate_seat_pmf = Pmf(res.senate_dem_seats,
                              static_cast<std::size_t>(config.senate.dem_seats_not_up) +
                                  layout.senate_races.size());
    res.p_senate_dem_control = Mean(res.senate_dem_control);
  }
  if (house) {
    res.house_dem_control.resize(n);
    for (std::size_t d = 0; d < n; ++d) {
      res.house_dem_control[d] = res.house_dem_seats[d] >= config.house.control_threshold ? 1 : 0;
    }
    res.house_seat_pmf = Pmf(res.house_dem_seats, config.districts.size());
    res.p_house_dem_control = Mean(res.house_dem_control);
  }
  return res;
}

}  // namespace susq::sim
