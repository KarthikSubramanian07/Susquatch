#include "susq/tourney/field.hpp"

#include <algorithm>
#include <cmath>
#include <map>
#include <stdexcept>
#include <string>

namespace susq::tourney {
namespace {

[[noreturn]] void Fail(const std::string& what) { throw std::invalid_argument("tourney: " + what); }

bool InUnit(double x) { return std::isfinite(x) && x >= 0.0 && x <= 1.0; }

// Adds `mass` to above/tie according to where `value` sits relative to m.
void Place(double value, double mass, double m, Placement& out) {
  if (SameMultiple(value, m)) {
    out.tie += mass;
  } else if (value > m) {
    out.above += mass;
  }
}

}  // namespace

bool SameMultiple(double a, double b) {
  return std::abs(a - b) <= 1e-9 * std::max({1.0, std::abs(a), std::abs(b)});
}

void ValidateParams(const FieldParams& p) {
  for (double f : {p.f_passive, p.f_kelly, p.f_yolo, p.f_bot}) {
    if (!InUnit(f)) Fail("type fractions must be in [0,1]");
  }
  if (std::abs(p.f_passive + p.f_kelly + p.f_yolo + p.f_bot - 1.0) > 1e-6) {
    Fail("type fractions must sum to 1");
  }
  if (!(p.kelly_lo >= 0.0 && p.kelly_lo <= p.kelly_hi && std::isfinite(p.kelly_hi))) {
    Fail("need 0 <= kelly_lo <= kelly_hi");
  }
  if (!(p.yolo_max_price > 0.0 && p.yolo_max_price <= 1.0)) Fail("yolo_max_price must be in (0,1]");
  if (!std::isfinite(p.yolo_price_exponent)) Fail("yolo_price_exponent must be finite");
  if (p.bot_rolls < 0 || p.bot_rolls > 64) Fail("bot_rolls must be in [0,64]");
  if (!(p.bot_roll_multiple >= 0.0 && std::isfinite(p.bot_roll_multiple))) Fail("bad bot_roll_multiple");
  if (!InUnit(p.bot_roll_success) || !InUnit(p.bot_fail_keep)) Fail("bot probabilities must be in [0,1]");
}

std::vector<std::pair<double, double>> BotPmf(const FieldParams& p) {
  std::map<double, double> merged;
  const int k = p.bot_rolls;
  for (int s = 0; s <= k; ++s) {
    // C(k,s) q^s (1-q)^(k-s), computed in floating point (k <= 64).
    double comb = 1.0;
    for (int i = 1; i <= s; ++i) comb = comb * (k - s + i) / i;
    const double prob = comb * std::pow(p.bot_roll_success, s) * std::pow(1.0 - p.bot_roll_success, k - s);
    const double value = std::pow(p.bot_roll_multiple, s) * std::pow(p.bot_fail_keep, k - s);
    if (prob > 0.0) merged[value] += prob;
  }
  return {merged.begin(), merged.end()};
}

std::vector<double> YoloWeights(const std::vector<double>& prices, const FieldParams& params) {
  std::vector<double> w(prices.size(), 0.0);
  double total = 0.0;
  for (std::size_t j = 0; j < prices.size(); ++j) {
    if (prices[j] <= params.yolo_max_price + 1e-12) {
      w[j] = std::pow(prices[j], params.yolo_price_exponent);
      total += w[j];
    }
  }
  if (total <= 0.0) {
    if (params.f_yolo > 0.0) Fail("f_yolo > 0 but no contract is priced <= yolo_max_price");
    return w;
  }
  for (double& x : w) x /= total;
  return w;
}

FieldModel::FieldModel(Market market, FieldParams params)
    : market_(std::move(market)), params_(params) {
  ValidateParams(params_);
  const std::size_t n_contracts = market_.prices.size();
  if (market_.n_worlds == 0) Fail("market needs at least one world");
  if (market_.pays.size() != n_contracts * market_.n_worlds) Fail("pays must be n_contracts x n_worlds");
  for (double p : market_.prices) {
    if (!(p > 0.0 && p < 1.0)) Fail("contract prices must be in (0,1)");
  }
  if (market_.world_weights.empty()) {
    weights_.assign(market_.n_worlds, 1.0 / static_cast<double>(market_.n_worlds));
  } else {
    if (market_.world_weights.size() != market_.n_worlds) Fail("world_weights must have n_worlds entries");
    double total = 0.0;
    for (double w : market_.world_weights) {
      if (!(w >= 0.0 && std::isfinite(w))) Fail("world weights must be finite and >= 0");
      total += w;
    }
    if (total <= 0.0) Fail("world weights sum to 0");
    weights_ = market_.world_weights;
    for (double& w : weights_) w /= total;
  }
  yolo_weights_ = YoloWeights(market_.prices, params_);
  bot_pmf_ = BotPmf(params_);
}

Placement FieldModel::CompareFixedTypes(double m) const {
  const FieldParams& p = params_;
  Placement out;
  Place(1.0, p.f_passive, m, out);
  if (p.f_kelly > 0.0) {
    if (p.kelly_hi > p.kelly_lo) {
      out.above += p.f_kelly * std::clamp((p.kelly_hi - m) / (p.kelly_hi - p.kelly_lo), 0.0, 1.0);
    } else {
      Place(p.kelly_lo, p.f_kelly, m, out);
    }
  }
  if (p.f_bot > 0.0) {
    for (const auto& [value, prob] : bot_pmf_) Place(value, p.f_bot * prob, m, out);
  }
  return out;
}

Placement FieldModel::Compare(std::size_t world, double m) const {
  const FieldParams& p = params_;
  Placement out = CompareFixedTypes(m);
  if (p.f_yolo > 0.0) {
    const std::size_t n_worlds = market_.n_worlds;
    for (std::size_t j = 0; j < yolo_weights_.size(); ++j) {
      if (yolo_weights_[j] == 0.0) continue;
      const double value = market_.pays[j * n_worlds + world] ? 1.0 / market_.prices[j] : 0.0;
      Place(value, p.f_yolo * yolo_weights_[j], m, out);
    }
  }
  out.above = std::clamp(out.above, 0.0, 1.0);
  out.tie = std::clamp(out.tie, 0.0, 1.0 - out.above);
  return out;
}

std::vector<double> FieldModel::Atoms(std::size_t world) const {
  std::vector<double> atoms{1.0};
  if (params_.f_kelly > 0.0) {
    atoms.push_back(params_.kelly_lo);
    atoms.push_back(params_.kelly_hi);
  }
  if (params_.f_yolo > 0.0) {
    for (std::size_t j = 0; j < yolo_weights_.size(); ++j) {
      if (yolo_weights_[j] == 0.0) continue;
      atoms.push_back(market_.pays[j * market_.n_worlds + world] ? 1.0 / market_.prices[j] : 0.0);
    }
  }
  if (params_.f_bot > 0.0) {
    for (const auto& atom : bot_pmf_) atoms.push_back(atom.first);
  }
  return atoms;
}

}  // namespace susq::tourney
