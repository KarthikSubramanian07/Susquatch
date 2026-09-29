#include <pybind11/numpy.h>
#include <pybind11/pybind11.h>
#include <pybind11/stl.h>

#include <optional>
#include <stdexcept>
#include <vector>

#include "kwargs_init.hpp"
#include "numpy_util.hpp"
#include "susq/tourney/fit.hpp"
#include "susq/tourney/tourney.hpp"

namespace susq::bindings {
namespace {

using namespace susq::tourney;

using DoubleArray = py::array_t<double, py::array::c_style | py::array::forcecast>;
using ByteArray = py::array_t<uint8_t, py::array::c_style | py::array::forcecast>;

std::vector<double> ToVector(const DoubleArray& a) {
  const auto* p = a.data();
  return {p, p + a.size()};
}

FieldModel MakeField(const DoubleArray& prices, const ByteArray& pays, const FieldParams& params,
                     const std::optional<DoubleArray>& world_weights) {
  if (prices.ndim() != 1) throw std::invalid_argument("prices must be 1-D");
  if (pays.ndim() != 2 || pays.shape(0) != prices.shape(0)) {
    throw std::invalid_argument("pays must be (n_contracts, n_worlds)");
  }
  Market market;
  market.prices = ToVector(prices);
  market.n_worlds = static_cast<std::size_t>(pays.shape(1));
  market.pays.assign(pays.data(), pays.data() + pays.size());
  if (world_weights) market.world_weights = ToVector(*world_weights);
  return FieldModel(std::move(market), params);
}

py::dict ParamsDict(const FieldParams& p) {
  py::dict d;
  d["n_players"] = p.n_players;
  d["f_passive"] = p.f_passive;
  d["f_kelly"] = p.f_kelly;
  d["f_yolo"] = p.f_yolo;
  d["f_bot"] = p.f_bot;
  d["kelly_lo"] = p.kelly_lo;
  d["kelly_hi"] = p.kelly_hi;
  return d;
}

}  // namespace

void BindTourney(py::module_& m) {
  py::class_<FieldParams> params(m, "FieldParams", "Field size and type mix (fractions sum to 1).");
  AddKwargsInit(params);
  params.def_readwrite("n_players", &FieldParams::n_players)
      .def_readwrite("f_passive", &FieldParams::f_passive)
      .def_readwrite("f_kelly", &FieldParams::f_kelly)
      .def_readwrite("f_yolo", &FieldParams::f_yolo)
      .def_readwrite("f_bot", &FieldParams::f_bot)
      .def_readwrite("kelly_lo", &FieldParams::kelly_lo)
      .def_readwrite("kelly_hi", &FieldParams::kelly_hi)
      .def_readwrite("yolo_max_price", &FieldParams::yolo_max_price)
      .def_readwrite("yolo_price_exponent", &FieldParams::yolo_price_exponent)
      .def_readwrite("bot_rolls", &FieldParams::bot_rolls)
      .def_readwrite("bot_roll_multiple", &FieldParams::bot_roll_multiple)
      .def_readwrite("bot_roll_success", &FieldParams::bot_roll_success)
      .def_readwrite("bot_fail_keep", &FieldParams::bot_fail_keep)
      .def("to_dict", &ParamsDict);

  py::class_<FieldModel>(m, "FieldModel", "Field + market: contract prices and which contracts pay per world.")
      .def(py::init(&MakeField), py::arg("prices"), py::arg("pays"), py::arg("params"),
           py::arg("world_weights") = std::nullopt,
           "prices: (n_contracts,) entry prices in (0,1); pays: (n_contracts, n_worlds) 0/1 "
           "(e.g. from sim outcomes); world_weights: optional (n_worlds,).")
      .def_property_readonly("params", &FieldModel::params)
      .def_property_readonly("yolo_weights", [](const FieldModel& f) { return ToArray(f.yolo_weights()); })
      .def(
          "compare",
          [](const FieldModel& f, std::size_t world, double m) {
            const Placement p = f.Compare(world, m);
            return py::make_tuple(p.above, p.tie);
          },
          py::arg("world"), py::arg("multiple"), "(P(player above m), P(player tied at m)) in one world.");

  m.def(
      "evaluate_book",
      [](const FieldModel& field, double cash, const std::vector<uint32_t>& contracts,
         const std::vector<double>& shares, double start_balance, const std::vector<double>& prizes,
         unsigned threads) {
        if (contracts.size() != shares.size()) throw std::invalid_argument("contracts and shares differ in length");
        Book book;
        book.cash = cash;
        book.start_balance = start_balance;
        for (std::size_t i = 0; i < contracts.size(); ++i) book.positions.push_back({contracts[i], shares[i]});
        Prizes p;
        p.amounts = prizes;
        BookEvaluation e;
        {
          py::gil_scoped_release release;
          e = EvaluateBook(field, book, p, threads);
        }
        py::dict d;
        d["p_first"] = e.p_first;
        d["p_top3"] = e.p_top3;
        d["expected_prize"] = e.expected_prize;
        d["multiple"] = ToArray(e.multiple);
        d["p_first_by_world"] = ToArray(e.p_first_by_world);
        return d;
      },
      py::arg("field"), py::arg("cash"), py::arg("contracts"), py::arg("shares"),
      py::arg("start_balance") = 100000.0, py::arg("prizes") = std::vector<double>{30000.0, 5000.0, 2500.0},
      py::arg("threads") = 0U,
      "P(1st), P(top 3) and expected prize of a final book (cash + YES shares), ties split per the "
      "Official Rules.");

  m.def("expected_prize_share", &ExpectedPrizeShare, py::arg("n_others"), py::arg("above"), py::arg("tie"),
        py::arg("prizes"));
  m.def("prob_first_at_multiple", &ProbFirstAtMultiple, py::arg("field"), py::arg("multiple"),
        py::call_guard<py::gil_scoped_release>());
  m.def(
      "threshold_cdf",
      [](const FieldModel& f, const std::vector<double>& m_values, unsigned threads) {
        std::vector<double> cdf;
        {
          py::gil_scoped_release release;
          cdf = ThresholdCdf(f, m_values, threads);
        }
        return ToArray(cdf);
      },
      py::arg("field"), py::arg("m_values"), py::arg("threads") = 0U, "P(field max T <= m) per requested m.");
  m.def(
      "threshold_quantiles",
      [](const FieldModel& f, const std::vector<double>& probs, unsigned threads) {
        std::vector<double> q;
        {
          py::gil_scoped_release release;
          q = ThresholdQuantiles(f, probs, threads);
        }
        return ToArray(q);
      },
      py::arg("field"), py::arg("probs"), py::arg("threads") = 0U, "Quantiles of the field maximum T.");

  py::class_<LeaderboardSnapshot> snap(m, "LeaderboardSnapshot");
  AddKwargsInit(snap);
  snap.def_readwrite("n_players", &LeaderboardSnapshot::n_players)
      .def_readwrite("top_multiples", &LeaderboardSnapshot::top_multiples)
      .def_readwrite("contract_values", &LeaderboardSnapshot::contract_values)
      .def_readwrite("entry_prices", &LeaderboardSnapshot::entry_prices)
      .def_readwrite("bots_active", &LeaderboardSnapshot::bots_active);

  m.def(
      "fit_field",
      [](const std::vector<LeaderboardSnapshot>& snaps, const FieldParams& base, std::vector<double> f_yolo_grid,
         std::vector<double> kelly_hi_grid) {
        FitOptions opts{std::move(f_yolo_grid), std::move(kelly_hi_grid)};
        FitResult r;
        {
          py::gil_scoped_release release;
          r = FitField(snaps, base, opts);
        }
        py::dict d;
        d["params"] = r.params;
        d["loss"] = r.loss;
        d["f_yolo_grid"] = ToArray(r.f_yolo_grid);
        d["loss_by_f_yolo"] = ToArray(r.loss_by_f_yolo);
        return d;
      },
      py::arg("snapshots"), py::arg("base"), py::arg("f_yolo_grid") = std::vector<double>{},
      py::arg("kelly_hi_grid") = std::vector<double>{},
      "Grid-search f_yolo (and optionally kelly_hi) by order-statistic quantile matching.");
}

}  // namespace susq::bindings
