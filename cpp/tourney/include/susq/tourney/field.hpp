// Competitor field model (SPEC §6, docs/MODELS.md §2).
//
// Every other player is, independently, one of four types:
//   passive        final multiple exactly 1.0 (qualifying trade, then nothing);
//   consensus      Kelly-ish diversified book, multiple ~ Uniform[kelly_lo, kelly_hi];
//   YOLO           all-in on one contract j with P(j) proportional to price_j^alpha among
//                  contracts priced <= yolo_max_price; ends at 1/p_j if j pays, else 0;
//   bot            election-night compounding: `bot_rolls` all-in rolls, each multiplying the
//                  bankroll by bot_roll_multiple with prob bot_roll_success, else by bot_fail_keep.
// Given a world (which contracts pay), players are i.i.d., so the field's order statistics have
// closed forms: P(T <= m | world) = G(m)^N, with G the per-player CDF.
#pragma once

#include <cstddef>
#include <cstdint>
#include <utility>
#include <vector>

namespace susq::tourney {

struct FieldParams {
  uint32_t n_players = 2000;     ///< Other ranked players (you excluded). Observable: leaderboard `total`.
  double f_passive = 0.55;
  double f_kelly = 0.30;
  double f_yolo = 0.10;
  double f_bot = 0.05;
  double kelly_lo = 0.8;
  double kelly_hi = 1.3;
  double yolo_max_price = 0.25;  ///< YOLOs only buy contracts at or below this price.
  double yolo_price_exponent = 1.0;  ///< alpha in P(j) ~ p_j^alpha ("price-weighted").
  int bot_rolls = 3;
  double bot_roll_multiple = 2.5;
  double bot_roll_success = 0.4;
  double bot_fail_keep = 0.0;    ///< Fraction kept when a roll fails (0 = all-in).
};

/// Contract prices and the world set: pays[c * n_worlds + w] = 1 if contract c resolves YES in w.
struct Market {
  std::vector<double> prices;           ///< Entry price per contract, in (0, 1).
  std::size_t n_worlds = 0;
  std::vector<uint8_t> pays;            ///< [contract][world]
  std::vector<double> world_weights;    ///< Empty = uniform; otherwise normalized internally.
};

/// Probability that one random field player finishes strictly above / exactly tied with m.
struct Placement {
  double above = 0.0;
  double tie = 0.0;
};

/// Validated field + market with precomputed YOLO weights and the bot multiple distribution.
class FieldModel {
 public:
  /// Throws std::invalid_argument on inconsistent inputs (fractions must sum to 1).
  FieldModel(Market market, FieldParams params);

  const Market& market() const { return market_; }
  const FieldParams& params() const { return params_; }
  double world_weight(std::size_t w) const { return weights_[w]; }

  /// Per-player placement relative to multiple m in world w.
  Placement Compare(std::size_t world, double m) const;

  /// The world-independent part of Compare: passive, consensus and bot players only
  /// (masses are weighted by their type fractions; the YOLO share is left out).
  Placement CompareFixedTypes(double m) const;

  /// Support points of the per-player multiple in world w (atoms of every type), unsorted.
  std::vector<double> Atoms(std::size_t world) const;

  /// YOLO contract choice probabilities (0 for ineligible contracts).
  const std::vector<double>& yolo_weights() const { return yolo_weights_; }

  /// (multiple, probability) atoms of the bot type.
  const std::vector<std::pair<double, double>>& bot_pmf() const { return bot_pmf_; }

 private:
  Market market_;
  FieldParams params_;
  std::vector<double> weights_;
  std::vector<double> yolo_weights_;
  std::vector<std::pair<double, double>> bot_pmf_;
};

/// True when |a - b| is within the tie tolerance used everywhere (1e-9 relative).
bool SameMultiple(double a, double b);

/// Validates fractions/ranges; throws std::invalid_argument.
void ValidateParams(const FieldParams& params);

/// Bot multiple distribution implied by the roll parameters.
std::vector<std::pair<double, double>> BotPmf(const FieldParams& params);

/// YOLO choice probabilities over contracts with the given entry prices.
std::vector<double> YoloWeights(const std::vector<double>& prices, const FieldParams& params);

}  // namespace susq::tourney
