#include <pybind11/numpy.h>
#include <pybind11/pybind11.h>
#include <pybind11/stl.h>

#include <optional>
#include <stdexcept>
#include <string>
#include <vector>

#include "kwargs_init.hpp"
#include "numpy_util.hpp"
#include "susq/sim/simulator.hpp"

namespace susq::bindings {
namespace {

using namespace susq::sim;

// Holds a config plus the latest world set and results; the Python-facing workflow object.
class Model {
 public:
  explicit Model(SimConfig config) : config_(std::move(config)) { Validate(config_); }

  void Generate() {
    worlds_ = sim::Generate(config_);
    results_.reset();
  }

  CalibrationReport Calibrate() { return sim::Calibrate(config_, RequireWorlds()); }

  void Evaluate() { results_ = sim::Evaluate(config_, RequireWorlds()); }

  const SimConfig& config() const { return config_; }
  void set_config(SimConfig config) {
    Validate(config);
    config_ = std::move(config);
    worlds_.reset();
    results_.reset();
  }

  const WorldSet& RequireWorlds() const {
    if (!worlds_) throw std::runtime_error("call generate() first");
    return *worlds_;
  }
  const Results& RequireResults() const {
    if (!results_) throw std::runtime_error("call evaluate() first");
    return *results_;
  }

 private:
  SimConfig config_;
  std::optional<WorldSet> worlds_;
  std::optional<Results> results_;
};

py::dict BundlesToDict(const ScenarioBundles& b, std::size_t n_races) {
  const std::size_t n_b = b.buckets.size();
  std::vector<int> decile(n_b);
  std::vector<uint32_t> signs(n_b);
  std::vector<uint64_t> n_draws(n_b);
  std::vector<double> prob(n_b), mean_n(n_b), senate(n_b), house(n_b);
  std::vector<double> p_dem(n_b * n_races), p_rep(n_b * n_races);
  for (std::size_t k = 0; k < n_b; ++k) {
    const ScenarioBucket& s = b.buckets[k];
    decile[k] = s.national_decile;
    signs[k] = s.cluster_signs;
    n_draws[k] = s.n_draws;
    prob[k] = s.probability;
    mean_n[k] = s.mean_national;
    senate[k] = s.p_senate_dem_control;
    house[k] = s.p_house_dem_control;
    std::copy(s.p_dem_market.begin(), s.p_dem_market.end(), p_dem.begin() + static_cast<std::ptrdiff_t>(k * n_races));
    std::copy(s.p_rep_market.begin(), s.p_rep_market.end(), p_rep.begin() + static_cast<std::ptrdiff_t>(k * n_races));
  }
  py::dict d;
  d["clusters"] = b.clusters;
  d["decile_edges"] = ToArray(b.decile_edges);
  d["national_decile"] = ToArray(decile);
  d["cluster_signs"] = ToArray(signs);
  d["n_draws"] = ToArray(n_draws);
  d["probability"] = ToArray(prob);
  d["mean_national"] = ToArray(mean_n);
  d["p_dem_market"] = ToArray2D(p_dem, n_b, n_races);
  d["p_rep_market"] = ToArray2D(p_rep, n_b, n_races);
  d["p_senate_dem_control"] = ToArray(senate);
  d["p_house_dem_control"] = ToArray(house);
  d["draw_bucket"] = ToArray(b.draw_bucket);
  return d;
}

}  // namespace

void BindSim(py::module_& m) {
  py::enum_<Chamber>(m, "Chamber")
      .value("NONE", Chamber::kNone)
      .value("SENATE", Chamber::kSenate)
      .value("HOUSE", Chamber::kHouse)
      .value("GOVERNOR", Chamber::kGovernor);

  py::class_<ShockParams> shocks(m, "ShockParams", "Shared-shock parameters in points of D-R margin.");
  AddKwargsInit(shocks);
  shocks.def_readwrite("national_sd", &ShockParams::national_sd)
      .def_readwrite("national_nu", &ShockParams::national_nu)
      .def_readwrite("region_sd", &ShockParams::region_sd)
      .def_readwrite("state_sd", &ShockParams::state_sd)
      .def_readwrite("miss_prob", &ShockParams::miss_prob)
      .def_readwrite("miss_size", &ShockParams::miss_size)
      .def_readwrite("miss_prob_dem", &ShockParams::miss_prob_dem);

  py::class_<ClusterSpec> cluster(m, "ClusterSpec");
  AddKwargsInit(cluster);
  cluster.def_readwrite("name", &ClusterSpec::name).def_readwrite("sd", &ClusterSpec::sd);

  py::class_<ClusterLoading> loading(m, "ClusterLoading");
  AddKwargsInit(loading);
  loading.def_readwrite("cluster", &ClusterLoading::cluster).def_readwrite("loading", &ClusterLoading::loading);

  py::class_<RaceSpec> race(m, "RaceSpec", "One traded race; margin = mu + shared shocks + eps.");
  AddKwargsInit(race);
  race.def_readwrite("id", &RaceSpec::id)
      .def_readwrite("state", &RaceSpec::state)
      .def_readwrite("chamber", &RaceSpec::chamber)
      .def_readwrite("mu", &RaceSpec::mu)
      .def_readwrite("sd", &RaceSpec::sd)
      .def_readwrite("national_beta", &RaceSpec::national_beta)
      .def_readwrite("clusters", &RaceSpec::clusters)
      .def_readwrite("p_third_party", &RaceSpec::p_third_party)
      .def_readwrite("dem_side_is_independent", &RaceSpec::dem_side_is_independent)
      .def_readwrite("dem_side_caucuses_dem", &RaceSpec::dem_side_caucuses_dem)
      .def_readwrite("target_p_win", &RaceSpec::target_p_win)
      .def("__repr__", [](const RaceSpec& r) { return "RaceSpec(id='" + r.id + "', mu=" + std::to_string(r.mu) + ")"; });

  py::class_<HouseDistrictSpec> district(m, "HouseDistrictSpec");
  AddKwargsInit(district);
  district.def_readwrite("id", &HouseDistrictSpec::id)
      .def_readwrite("state", &HouseDistrictSpec::state)
      .def_readwrite("lean", &HouseDistrictSpec::lean)
      .def_readwrite("incumbency", &HouseDistrictSpec::incumbency)
      .def_readwrite("clusters", &HouseDistrictSpec::clusters)
      .def_readwrite("race_id", &HouseDistrictSpec::race_id);

  py::class_<SenateSpec> senate(m, "SenateSpec");
  AddKwargsInit(senate);
  senate.def_readwrite("dem_seats_not_up", &SenateSpec::dem_seats_not_up)
      .def_readwrite("control_threshold", &SenateSpec::control_threshold)
      .def_readwrite("vp_breaks_ties_dem", &SenateSpec::vp_breaks_ties_dem);

  py::class_<HouseSpec> house(m, "HouseSpec");
  AddKwargsInit(house);
  house.def_readwrite("generic_ballot", &HouseSpec::generic_ballot)
      .def_readwrite("lv_adjustment", &HouseSpec::lv_adjustment)
      .def_readwrite("national_extra_sd", &HouseSpec::national_extra_sd)
      .def_readwrite("district_sd", &HouseSpec::district_sd)
      .def_readwrite("control_threshold", &HouseSpec::control_threshold);

  py::class_<SimConfig> config(m, "SimConfig",
                               "Simulator inputs. List fields are copied on access: assign whole lists "
                               "(cfg.races = [...]) rather than appending in place.");
  AddKwargsInit(config);
  config.def_readwrite("shocks", &SimConfig::shocks)
      .def_readwrite("regions", &SimConfig::regions)
      .def_readwrite("state_region", &SimConfig::state_region)
      .def_readwrite("clusters", &SimConfig::clusters)
      .def_readwrite("races", &SimConfig::races)
      .def_readwrite("districts", &SimConfig::districts)
      .def_readwrite("senate", &SimConfig::senate)
      .def_readwrite("house", &SimConfig::house)
      .def_readwrite("seed", &SimConfig::seed)
      .def_readwrite("n_draws", &SimConfig::n_draws)
      .def_readwrite("threads", &SimConfig::threads);

  m.def("default_config", &DefaultConfig, "Census regions + SPEC §5 shock defaults.");
  m.def("default_state_regions", &DefaultStateRegions);
  m.attr("BLOCK_SIZE") = kBlockSize;
  m.attr("REP_WINS") = static_cast<int>(kRepWins);
  m.attr("DEM_SIDE_WINS") = static_cast<int>(kDemSideWins);
  m.attr("THIRD_PARTY_WINS") = static_cast<int>(kThirdPartyWins);

  py::class_<Model>(m, "Model", "Generate -> calibrate -> evaluate workflow over one SimConfig.")
      .def(py::init<SimConfig>(), py::arg("config"))
      .def_property(
          "config", [](const Model& s) { return s.config(); }, &Model::set_config,
          "A copy of the config; assign a modified copy back to apply it (drops the world set).")
      .def("generate", &Model::Generate, py::call_guard<py::gil_scoped_release>(),
           "Draw the mu-independent world set (deterministic in config.seed).")
      .def(
          "calibrate",
          [](Model& self) {
            CalibrationReport rep;
            {
              py::gil_scoped_release release;
              rep = self.Calibrate();
            }
            py::list out;
            for (const auto& e : rep.entries) {
              py::dict d;
              d["race_id"] = e.race_id;
              d["target"] = e.target;
              d["achieved"] = e.achieved;
              d["mu"] = e.mu;
              d["feasible"] = e.feasible;
              out.append(d);
            }
            return out;
          },
          "Solve each targeted race's mu on the current world set; returns one dict per race.")
      .def("evaluate", &Model::Evaluate, py::call_guard<py::gil_scoped_release>())
      .def(
          "run",
          [](Model& self) {
            py::gil_scoped_release release;
            self.Generate();
            self.Calibrate();
            self.Evaluate();
          },
          "generate() + calibrate() + evaluate().")
      .def_property_readonly("n_draws", [](const Model& s) { return s.RequireWorlds().n_draws; })
      .def_property_readonly("race_ids",
                             [](const Model& s) {
                               std::vector<std::string> ids;
                               for (const auto& r : s.config().races) ids.push_back(r.id);
                               return ids;
                             })
      .def_property_readonly("mu",
                             [](const Model& s) {
                               std::vector<double> mu;
                               for (const auto& r : s.config().races) mu.push_back(r.mu);
                               return ToArray(mu);
                             })
      .def_property_readonly("national", [](const Model& s) { return ToArray(s.RequireWorlds().national); })
      .def_property_readonly("cluster_shocks",
                             [](const Model& s) {
                               const auto& w = s.RequireWorlds();
                               return ToArray2D(w.cluster_shocks, s.config().clusters.size(), w.n_draws);
                             })
      .def_property_readonly("race_errors",
                             [](const Model& s) {
                               const auto& w = s.RequireWorlds();
                               return ToArray2D(w.race_errors, s.config().races.size(), w.n_draws);
                             })
      .def_property_readonly("outcomes",
                             [](const Model& s) {
                               const auto& r = s.RequireResults();
                               return ToArray2D(r.outcomes, s.config().races.size(), r.n_draws);
                             },
                             "(n_races, n_draws) uint8: 0 = R wins, 1 = D side wins, 2 = third party.")
      .def_property_readonly("p_dem_side", [](const Model& s) { return ToArray(s.RequireResults().p_dem_side); })
      .def_property_readonly("p_dem_market", [](const Model& s) { return ToArray(s.RequireResults().p_dem_market); })
      .def_property_readonly("p_rep_market", [](const Model& s) { return ToArray(s.RequireResults().p_rep_market); })
      .def_property_readonly("senate_dem_seats",
                             [](const Model& s) { return ToArray(s.RequireResults().senate_dem_seats); })
      .def_property_readonly("house_dem_seats", [](const Model& s) { return ToArray(s.RequireResults().house_dem_seats); })
      .def_property_readonly("senate_seat_pmf", [](const Model& s) { return ToArray(s.RequireResults().senate_seat_pmf); })
      .def_property_readonly("house_seat_pmf", [](const Model& s) { return ToArray(s.RequireResults().house_seat_pmf); })
      .def_property_readonly("p_senate_dem_control", [](const Model& s) { return s.RequireResults().p_senate_dem_control; })
      .def_property_readonly("p_house_dem_control", [](const Model& s) { return s.RequireResults().p_house_dem_control; })
      .def(
          "scenario_bundles",
          [](const Model& self, const std::vector<std::string>& clusters) {
            ScenarioBundles b;
            {
              py::gil_scoped_release release;
              b = BuildScenarioBundles(self.config(), self.RequireWorlds(), self.RequireResults(), clusters);
            }
            return BundlesToDict(b, self.config().races.size());
          },
          py::arg("clusters"),
          "Buckets by national decile x cluster-shock signs; struct-of-arrays dict (bucket index = "
          "decile * 2**len(clusters) + signs).");
}

}  // namespace susq::bindings
