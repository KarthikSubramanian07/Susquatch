// Flat-array entry points for the research pipeline's engine facade (py/susq/models/facade.py):
// SIM_API_VERSION = 1 `simulate_margins` and TOURNEY_API_VERSION = 1 `simulate_threshold`, with the
// semantics of the numpy reference engine. Same distributions, different random numbers.
#include <pybind11/numpy.h>
#include <pybind11/pybind11.h>

#include <stdexcept>
#include <string>
#include <vector>

#include "numpy_util.hpp"
#include "susq/sim/simulator.hpp"
#include "susq/tourney/sampled.hpp"

namespace susq::bindings {
namespace {

using DoubleArray = py::array_t<double, py::array::c_style | py::array::forcecast>;
using IntArray = py::array_t<int32_t, py::array::c_style | py::array::forcecast>;
using ByteArray = py::array_t<uint8_t, py::array::c_style | py::array::forcecast>;

py::tuple SimulateMargins(const DoubleArray& mu, const DoubleArray& beta, const DoubleArray& race_sd,
                          const IntArray& region_idx, const IntArray& state_idx, const DoubleArray& membership,
                          const DoubleArray& cluster_sd, int n_regions, int n_states, double nu, double national_sd,
                          double region_sd, double state_sd, double miss_p, double miss_shift, std::size_t n_draws,
                          uint64_t seed) {
  const auto k = static_cast<std::size_t>(mu.size());
  const auto n_clusters = static_cast<std::size_t>(cluster_sd.size());
  if (static_cast<std::size_t>(beta.size()) != k || static_cast<std::size_t>(race_sd.size()) != k ||
      static_cast<std::size_t>(region_idx.size()) != k || static_cast<std::size_t>(state_idx.size()) != k) {
    throw std::invalid_argument("simulate_margins: per-race arrays must all have len(mu)");
  }
  if (membership.ndim() != 2 || static_cast<std::size_t>(membership.shape(0)) != k ||
      static_cast<std::size_t>(membership.shape(1)) != n_clusters) {
    throw std::invalid_argument("simulate_margins: membership must be (len(mu), len(cluster_sd))");
  }

  sim::SimConfig cfg;
  cfg.shocks.national_nu = nu;
  cfg.shocks.national_sd = national_sd;
  cfg.shocks.region_sd = region_sd;
  cfg.shocks.state_sd = state_sd;
  cfg.shocks.miss_prob = miss_p;
  cfg.shocks.miss_size = miss_shift;
  cfg.n_draws = n_draws;
  cfg.seed = seed;
  for (int r = 0; r < n_regions; ++r) cfg.regions.push_back("region-" + std::to_string(r));
  for (std::size_t c = 0; c < n_clusters; ++c) cfg.clusters.push_back({"cluster-" + std::to_string(c), cluster_sd.at(c)});
  for (std::size_t i = 0; i < k; ++i) {
    sim::RaceSpec race;
    race.id = "race-" + std::to_string(i);
    race.mu = mu.at(i);
    race.national_beta = beta.at(i);
    race.sd = race_sd.at(i);
    const int st = state_idx.at(i);
    const int rg = region_idx.at(i);
    if (st >= n_states || rg >= n_regions) throw std::invalid_argument("simulate_margins: index out of range");
    if (st >= 0) {
      if (rg < 0) throw std::invalid_argument("simulate_margins: a race with a state needs a region");
      race.state = "state-" + std::to_string(st);
      const std::string region = "region-" + std::to_string(rg);
      auto [it, inserted] = cfg.state_region.emplace(race.state, region);
      if (!inserted && it->second != region) {
        throw std::invalid_argument("simulate_margins: one state index maps to two regions");
      }
    } else if (rg >= 0) {
      throw std::invalid_argument("simulate_margins: region without a state is not supported");
    }
    for (std::size_t c = 0; c < n_clusters; ++c) {
      const double w = membership.at(i, c);
      if (w != 0.0) race.clusters.push_back({"cluster-" + std::to_string(c), w});
    }
    cfg.races.push_back(std::move(race));
  }

  sim::WorldSet worlds;
  {
    py::gil_scoped_release release;
    worlds = sim::Generate(cfg);
  }
  // (n_draws, k) float64, margins include mu; the C++ layout is race-major float32.
  py::array_t<double> margins({static_cast<py::ssize_t>(n_draws), static_cast<py::ssize_t>(k)});
  double* m = margins.mutable_data();
  for (std::size_t i = 0; i < k; ++i) {
    const float* e = &worlds.race_errors[i * n_draws];
    const double mu_i = cfg.races[i].mu;
    for (std::size_t d = 0; d < n_draws; ++d) m[d * k + i] = mu_i + static_cast<double>(e[d]);
  }
  py::array_t<double> national(static_cast<py::ssize_t>(n_draws));
  std::copy(worlds.national.begin(), worlds.national.end(), national.mutable_data());
  py::array_t<double> clusters({static_cast<py::ssize_t>(n_draws), static_cast<py::ssize_t>(n_clusters)});
  double* cl = clusters.mutable_data();
  for (std::size_t c = 0; c < n_clusters; ++c) {
    for (std::size_t d = 0; d < n_draws; ++d) cl[d * n_clusters + c] = worlds.cluster_shocks[c * n_draws + d];
  }
  return py::make_tuple(margins, national, clusters);
}

py::array_t<double> SimulateThreshold(const ByteArray& outcomes, const DoubleArray& prices, uint32_t n_players,
                                      double frac_kelly, double frac_yolo, double frac_bot, double kelly_lo,
                                      double kelly_hi, double yolo_price_power, double bot_log_mean,
                                      double bot_log_sd, uint64_t seed) {
  if (outcomes.ndim() != 2 || outcomes.shape(1) != prices.size()) {
    throw std::invalid_argument("simulate_threshold: outcomes must be (n_worlds, len(prices))");
  }
  tourney::SampledFieldParams p;
  p.n_players = n_players;
  p.frac_kelly = frac_kelly;
  p.frac_yolo = frac_yolo;
  p.frac_bot = frac_bot;
  p.kelly_lo = kelly_lo;
  p.kelly_hi = kelly_hi;
  p.yolo_price_power = yolo_price_power;
  p.bot_log_mean = bot_log_mean;
  p.bot_log_sd = bot_log_sd;
  const std::vector<double> price_vec(prices.data(), prices.data() + prices.size());
  std::vector<double> t;
  {
    py::gil_scoped_release release;
    t = tourney::SampleThresholds(outcomes.data(), static_cast<std::size_t>(outcomes.shape(0)), price_vec, p, seed);
  }
  return ToArray(t);
}

}  // namespace

void BindResearchApi(py::module_& m) {
  m.attr("SIM_API_VERSION") = 1;
  m.def("simulate_margins", &SimulateMargins, py::arg("mu"), py::arg("beta"), py::arg("race_sd"),
        py::arg("region_idx"), py::arg("state_idx"), py::arg("membership"), py::arg("cluster_sd"),
        py::arg("n_regions"), py::arg("n_states"), py::arg("nu"), py::arg("national_sd"), py::arg("region_sd"),
        py::arg("state_sd"), py::arg("miss_p"), py::arg("miss_shift"), py::arg("n_draws"), py::arg("seed"),
        "Flat-array sim for susq.models.facade: returns (margins[n, k], national[n], clusters[n, c]); "
        "index -1 = no region/state.");
  m.attr("TOURNEY_API_VERSION") = 1;
  m.def("simulate_threshold", &SimulateThreshold, py::arg("outcomes"), py::arg("prices"), py::arg("n_players"),
        py::arg("frac_kelly"), py::arg("frac_yolo"), py::arg("frac_bot"), py::arg("kelly_lo"), py::arg("kelly_hi"),
        py::arg("yolo_price_power"), py::arg("bot_log_mean"), py::arg("bot_log_sd"), py::arg("seed"),
        "One sampled field maximum T per world (semantics of susq.models.tourney.simulate_threshold).");
}

}  // namespace susq::bindings
