// susq_models: Python bindings for the C++ models (the only C++ <-> Python boundary).
//   import susq_models
//   from susq_models import sim, tourney, needle
#include <pybind11/pybind11.h>

#include "numpy_util.hpp"

namespace py = pybind11;

PYBIND11_MODULE(susq_models, m) {
  m.doc() = "Susquatch C++ models: correlated race Monte Carlo (sim), tournament field (tourney), "
            "election-night county model (needle).";
  auto sys_modules = py::module_::import("sys").attr("modules");
  auto sim = m.def_submodule("sim", "Correlated race Monte Carlo (SPEC §5).");
  susq::bindings::BindSim(sim);
  sys_modules["susq_models.sim"] = sim;
  auto tourney = m.def_submodule("tourney", "Competitor field and P(1st) model (SPEC §6).");
  susq::bindings::BindTourney(tourney);
  sys_modules["susq_models.tourney"] = tourney;
  auto needle = m.def_submodule("needle", "Election-night county expected-vote model.");
  susq::bindings::BindNeedle(needle);
  sys_modules["susq_models.needle"] = needle;
  m.attr("__version__") = "0.1.0";
}
