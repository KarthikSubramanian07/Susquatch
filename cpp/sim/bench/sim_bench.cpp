// Throughput check for the SPEC §5 target: 1e6 draws x ~60 races "in seconds", multithreaded.
// Usage: sim_bench [n_draws=1000000] [threads=0] [n_races=60] [n_districts=435]
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <string>

#include "susq/sim/simulator.hpp"

namespace {

using Clock = std::chrono::steady_clock;

double Seconds(Clock::time_point since) {
  return std::chrono::duration<double>(Clock::now() - since).count();
}

susq::sim::SimConfig BenchConfig(std::size_t n_draws, unsigned threads, int n_races, int n_districts) {
  using namespace susq::sim;
  SimConfig cfg = DefaultConfig();
  cfg.n_draws = n_draws;
  cfg.threads = threads;
  cfg.clusters = {{"red-state-reach", 2.0}, {"new-england-incumbency", 2.0},
                  {"midwest-industrial", 1.5}, {"house-suburban", 2.0}};
  const char* const states[] = {"OH", "IA", "ME", "TX", "AK", "KS", "MI", "NH", "SC", "NE",
                                "GA", "NC", "AZ", "NV", "PA", "WI", "MN", "CO", "VA", "NY"};
  for (int i = 0; i < n_races; ++i) {
    RaceSpec r;
    r.id = "race-" + std::to_string(i);
    r.state = states[i % 20];
    r.chamber = i < 35 ? Chamber::kSenate : Chamber::kGovernor;
    r.sd = 2.5 + 0.02 * (i % 50);
    r.target_p_win = 0.05 + 0.9 * static_cast<double>(i) / static_cast<double>(n_races);
    r.clusters.push_back({i % 2 == 0 ? "red-state-reach" : "midwest-industrial", 1.0});
    cfg.races.push_back(r);
  }
  cfg.senate.dem_seats_not_up = 34;
  for (int j = 0; j < n_districts; ++j) {
    HouseDistrictSpec d;
    d.id = "D-" + std::to_string(j);
    d.state = states[j % 20];
    d.lean = -30.0 + 60.0 * static_cast<double>(j) / static_cast<double>(n_districts);
    if (j % 7 == 0) d.clusters.push_back({"house-suburban", 1.0});
    cfg.districts.push_back(d);
  }
  return cfg;
}

}  // namespace

int main(int argc, char** argv) {
  const std::size_t n_draws = argc > 1 ? std::strtoull(argv[1], nullptr, 10) : 1000000;
  const auto threads = static_cast<unsigned>(argc > 2 ? std::strtoul(argv[2], nullptr, 10) : 0);
  const int n_races = argc > 3 ? std::atoi(argv[3]) : 60;
  const int n_districts = argc > 4 ? std::atoi(argv[4]) : 435;

  susq::sim::SimConfig cfg = BenchConfig(n_draws, threads, n_races, n_districts);
  auto t0 = Clock::now();
  const susq::sim::WorldSet worlds = susq::sim::Generate(cfg);
  const double t_gen = Seconds(t0);
  t0 = Clock::now();
  const auto report = susq::sim::Calibrate(cfg, worlds);
  const double t_cal = Seconds(t0);
  t0 = Clock::now();
  const auto results = susq::sim::Evaluate(cfg, worlds);
  const double t_eval = Seconds(t0);
  t0 = Clock::now();
  const auto bundles =
      susq::sim::BuildScenarioBundles(cfg, worlds, results, {"red-state-reach", "midwest-industrial"});
  const double t_bundle = Seconds(t0);

  std::printf("draws=%zu races=%d districts=%d threads=%u\n", n_draws, n_races, n_districts, threads);
  std::printf("generate  %.3fs\ncalibrate %.3fs (max |err| %.5f)\nevaluate  %.3fs\nbundles   %.3fs\n",
              t_gen, t_cal, report.max_abs_error, t_eval, t_bundle);
  std::printf("total     %.3fs   P(Senate D)=%.4f  P(House D)=%.4f  buckets=%zu\n",
              t_gen + t_cal + t_eval + t_bundle, results.p_senate_dem_control,
              results.p_house_dem_control, bundles.buckets.size());
  return 0;
}
