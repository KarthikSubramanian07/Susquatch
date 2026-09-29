#include <pybind11/pybind11.h>
#include <pybind11/stl.h>

#include <stdexcept>
#include <string>

#include "kwargs_init.hpp"
#include "numpy_util.hpp"
#include "susq/contracts.hpp"
#include "susq/needle/model.hpp"
#include "susq/needle/service.hpp"

namespace susq::bindings {
namespace {

using namespace susq::needle;

std::string JsonDumps(const py::object& obj) {
  return py::module_::import("json").attr("dumps")(obj).cast<std::string>();
}

susq::v1::CountyResultsBatch ParseBatchJson(const std::string& json) {
  susq::v1::CountyResultsBatch batch;
  if (!susq::contracts::FromJson(json, &batch)) throw py::value_error("not a CountyResultsBatch (proto3 JSON)");
  return batch;
}

py::dict RaceDict(const RaceEstimate& e) {
  py::dict d;
  d["race_id"] = e.race_id;
  d["p_dem"] = e.p_dem;
  d["p_rep"] = e.p_rep;
  d["margin_mean"] = e.margin_mean;
  d["margin_sd"] = e.margin_sd;
  d["expected_remaining_votes"] = e.expected_remaining_votes;
  d["reported_frac"] = e.reported_frac;
  d["n_batches"] = e.n_batches;
  d["n_counties_reporting"] = e.n_counties_reporting;
  return d;
}

}  // namespace

void BindNeedle(py::module_& m) {
  py::class_<NeedleParams> params(m, "NeedleParams");
  AddKwargsInit(params);
  params.def_readwrite("within_type_sd", &NeedleParams::within_type_sd)
      .def_readwrite("type_sd", &NeedleParams::type_sd)
      .def_readwrite("race_level_sd_floor", &NeedleParams::race_level_sd_floor)
      .def_readwrite("default_two_party_share", &NeedleParams::default_two_party_share)
      .def_readwrite("margin_sd_floor", &NeedleParams::margin_sd_floor)
      .def_readwrite("obs_var_floor", &NeedleParams::obs_var_floor)
      .def_readwrite("max_county_votes", &NeedleParams::max_county_votes);

  py::class_<NeedleModel>(m, "NeedleModel",
                          "The same model the susq-needle service runs; feed it replayed county results.")
      .def(py::init([](const std::string& prior_json, const NeedleParams& p) {
             return NeedleModel(ParsePrior(prior_json), p);
           }),
           py::arg("prior_json"), py::arg("params") = NeedleParams{}, "From NeedlePrior proto3 JSON text.")
      .def_static(
          "from_prior_bytes",
          [](const py::bytes& data, const NeedleParams& p) {
            susq::v1::NeedlePrior prior;
            if (!susq::contracts::Decode(std::string(data), &prior)) throw py::value_error("not a binary NeedlePrior");
            return NeedleModel(prior, p);
          },
          py::arg("data"), py::arg("params") = NeedleParams{})
      .def(
          "ingest",
          [](NeedleModel& self, const py::object& result, bool is_correction) {
            susq::v1::CountyResult r;
            const std::string json = py::isinstance<py::str>(result) ? result.cast<std::string>() : JsonDumps(result);
            if (!susq::contracts::FromJson(json, &r)) throw py::value_error("not a CountyResult");
            return std::string(ToString(self.Ingest(r, is_correction)));
          },
          py::arg("result"), py::arg("is_correction") = false,
          "Store one cumulative CountyResult (dict or JSON). Returns the ingest status; call "
          "recompute() before reading posteriors.")
      .def(
          "ingest_batch_bytes",
          [](NeedleModel& self, const py::bytes& data) {
            susq::v1::CountyResultsBatch batch;
            if (!susq::contracts::Decode(std::string(data), &batch)) {
              throw py::value_error("not a binary CountyResultsBatch");
            }
            return self.IngestBatch(batch);
          },
          py::arg("data"), "Ingest a binary CountyResultsBatch and recompute; returns results accepted.")
      .def(
          "ingest_batch",
          [](NeedleModel& self, const py::object& batch) {
            const std::string json = py::isinstance<py::str>(batch) ? batch.cast<std::string>() : JsonDumps(batch);
            return self.IngestBatch(ParseBatchJson(json));
          },
          py::arg("batch"), "Ingest a CountyResultsBatch (dict or proto3 JSON) and recompute.")
      .def("recompute", &NeedleModel::Recompute)
      .def("races",
           [](const NeedleModel& self) {
             py::list out;
             for (const auto& e : self.races()) out.append(RaceDict(e));
             return out;
           })
      .def("national",
           [](const NeedleModel& self) {
             py::dict d;
             d["mean"] = self.national().mean;
             d["sd"] = self.national().sd;
             d["n_races_informing"] = self.national().n_races_informing;
             return d;
           })
      .def("counties",
           [](const NeedleModel& self) {
             py::list out;
             for (const auto& c : self.Counties()) {
               py::dict d;
               d["race_id"] = c.race_id;
               d["county_fips"] = c.county_fips;
               d["modeled"] = c.modeled;
               d["reported_votes"] = c.reported_votes;
               d["reported_margin"] = c.reported_margin;
               d["expected_remaining_votes"] = c.expected_remaining_votes;
               d["remaining_margin"] = c.remaining_margin;
               d["county_deviation"] = c.county_deviation;
               out.append(d);
             }
             return out;
           })
      .def("counters",
           [](const NeedleModel& self) {
             const auto& c = self.counters();
             py::dict d;
             d["accepted"] = c.accepted;
             d["unchanged"] = c.unchanged;
             d["duplicate_batch"] = c.duplicate_batch;
             d["regression"] = c.regression;
             d["unknown_race"] = c.unknown_race;
             d["unmodeled_county"] = c.unmodeled_county;
             d["invalid"] = c.invalid;
             return d;
           })
      .def("race_level_sd", &NeedleModel::RaceLevelSd, py::arg("race_id"))
      .def(
          "race_posteriors_bytes",
          [](const NeedleModel& self) {
            susq::v1::RacePosteriors msg;
            self.FillRacePosteriors(&msg);
            return py::bytes(susq::contracts::Encode(msg));
          },
          "Binary susq.v1.RacePosteriors (header left unset).")
      .def("national_shift_bytes", [](const NeedleModel& self) {
        susq::v1::NationalShift msg;
        self.FillNationalShift(&msg);
        return py::bytes(susq::contracts::Encode(msg));
      });
}

}  // namespace susq::bindings
