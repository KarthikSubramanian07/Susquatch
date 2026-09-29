#include "susq/tourney/fit.hpp"

#include <algorithm>
#include <cmath>
#include <functional>
#include <limits>
#include <stdexcept>
#include <string>

namespace susq::tourney {
namespace {

[[noreturn]] void Fail(const std::string& what) { throw std::invalid_argument("tourney fit: " + what); }

void ValidateSnapshot(const LeaderboardSnapshot& s) {
  if (s.n_players == 0) Fail("snapshot with n_players == 0");
  if (s.top_multiples.empty() || s.top_multiples.size() > s.n_players) {
    Fail("top_multiples must be non-empty and at most n_players long");
  }
  if (s.contract_values.size() != s.entry_prices.size()) Fail("contract_values/entry_prices size mismatch");
  for (double p : s.entry_prices) {
    if (!(p > 0.0 && p < 1.0)) Fail("entry prices must be in (0,1)");
  }
  for (double v : s.contract_values) {
    if (!(v >= 0.0 && std::isfinite(v))) Fail("contract values must be finite and >= 0");
  }
  for (double x : s.top_multiples) {
    if (!(x >= 0.0 && std::isfinite(x))) Fail("top multiples must be finite and >= 0");
  }
}

// Adds mass/2 on a tie and mass when strictly above: the mid-survival contribution.
double MidAbove(double value, double mass, double x) {
  if (SameMultiple(value, x)) return 0.5 * mass;
  return value > x ? mass : 0.0;
}

}  // namespace

FieldParams WithYoloFraction(const FieldParams& base, double f_yolo) {
  FieldParams p = base;
  const double others = base.f_passive + base.f_kelly + base.f_bot;
  p.f_yolo = f_yolo;
  if (others > 0.0) {
    const double scale = (1.0 - f_yolo) / others;
    p.f_passive = base.f_passive * scale;
    p.f_kelly = base.f_kelly * scale;
    p.f_bot = base.f_bot * scale;
  } else {
    p.f_passive = 1.0 - f_yolo;
    p.f_kelly = 0.0;
    p.f_bot = 0.0;
  }
  return p;
}

double SnapshotSurvival(const LeaderboardSnapshot& snap, const FieldParams& params, double x) {
  double s = MidAbove(1.0, params.f_passive, x);
  if (params.f_kelly > 0.0) {
    if (params.kelly_hi > params.kelly_lo) {
      s += params.f_kelly * std::clamp((params.kelly_hi - x) / (params.kelly_hi - params.kelly_lo), 0.0, 1.0);
    } else {
      s += MidAbove(params.kelly_lo, params.f_kelly, x);
    }
  }
  if (params.f_bot > 0.0) {
    if (snap.bots_active) {
      for (const auto& [value, prob] : BotPmf(params)) s += MidAbove(value, params.f_bot * prob, x);
    } else {
      s += MidAbove(1.0, params.f_bot, x);
    }
  }
  if (params.f_yolo > 0.0) {
    const std::vector<double> w = YoloWeights(snap.entry_prices, params);
    for (std::size_t j = 0; j < w.size(); ++j) {
      if (w[j] > 0.0) s += MidAbove(snap.contract_values[j], params.f_yolo * w[j], x);
    }
  }
  return s;
}

double FitLoss(const std::vector<LeaderboardSnapshot>& snaps, const FieldParams& params) {
  double loss = 0.0;
  for (const auto& snap : snaps) {
    std::vector<double> top = snap.top_multiples;
    std::sort(top.begin(), top.end(), std::greater<>());
    const double n = snap.n_players;
    for (std::size_t i = 0; i < top.size(); ++i) {
      const double expected_rank = static_cast<double>(i) + 0.5;  // (i+1) - 1/2
      const double model_rank = std::max(n * SnapshotSurvival(snap, params, top[i]), 1e-9);
      const double r = std::log(model_rank) - std::log(expected_rank);
      loss += r * r;
    }
  }
  return loss;
}

FitResult FitField(const std::vector<LeaderboardSnapshot>& snaps, const FieldParams& base,
                   const FitOptions& options) {
  if (snaps.empty()) Fail("no snapshots");
  for (const auto& s : snaps) ValidateSnapshot(s);
  ValidateParams(base);

  std::vector<double> f_grid = options.f_yolo_grid;
  if (f_grid.empty()) {
    for (int i = 0; i <= 100; ++i) f_grid.push_back(0.005 * i);
  }
  std::vector<double> hi_grid = options.kelly_hi_grid;
  if (hi_grid.empty()) hi_grid.push_back(base.kelly_hi);

  FitResult best;
  best.loss = std::numeric_limits<double>::infinity();
  best.f_yolo_grid = f_grid;
  for (double hi : hi_grid) {
    if (hi < base.kelly_lo) Fail("kelly_hi grid value below kelly_lo");
    std::vector<double> profile;
    double best_here = std::numeric_limits<double>::infinity();
    FieldParams best_params = base;
    for (double f : f_grid) {
      if (!(f >= 0.0 && f <= 1.0)) Fail("f_yolo grid values must be in [0,1]");
      FieldParams p = WithYoloFraction(base, f);
      p.kelly_hi = hi;
      const double loss = FitLoss(snaps, p);
      profile.push_back(loss);
      if (loss < best_here) {
        best_here = loss;
        best_params = p;
      }
    }
    if (best_here < best.loss) {
      best.loss = best_here;
      best.params = best_params;
      best.loss_by_f_yolo = profile;
    }
  }
  return best;
}

}  // namespace susq::tourney
