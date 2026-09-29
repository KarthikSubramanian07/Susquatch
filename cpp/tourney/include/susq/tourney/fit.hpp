// Fitting the field mix to observed leaderboard statistics (ITERATION §2.2).
//
// Each snapshot gives the field size N and the top-k balances as multiples of 100k. Under the
// field model the i-th largest of N i.i.d. multiples sits near the (i - 1/2)/N survival quantile,
// so we minimize sum_i (log(N * S(x_i)) - log(i - 1/2))^2 over a parameter grid, where S is the
// per-player mid-survival P(X > x) + P(X = x)/2 at that snapshot's contract values.
#pragma once

#include <cstdint>
#include <vector>

#include "susq/tourney/field.hpp"

namespace susq::tourney {

struct LeaderboardSnapshot {
  uint32_t n_players = 0;               ///< leaderboard `total` at snapshot time
  std::vector<double> top_multiples;    ///< top-k balances / 100k, any order
  /// Multiple a YOLO holding contract j has at snapshot time: mark_j / entry_j before
  /// resolution, 1/entry_j or 0 after.
  std::vector<double> contract_values;
  std::vector<double> entry_prices;     ///< prices YOLOs paid (sets the choice weights)
  bool bots_active = false;             ///< false = bots still at 1.0 (before election night)
};

struct FitOptions {
  std::vector<double> f_yolo_grid;      ///< empty = 0.000, 0.005, ..., 0.500
  std::vector<double> kelly_hi_grid;    ///< empty = keep base.kelly_hi
};

struct FitResult {
  FieldParams params;                   ///< base with the best f_yolo (others rescaled) and kelly_hi
  double loss = 0.0;
  std::vector<double> f_yolo_grid;
  std::vector<double> loss_by_f_yolo;   ///< profile loss at the best kelly_hi
};

/// Per-player mid-survival S(x) at a snapshot under `params`.
double SnapshotSurvival(const LeaderboardSnapshot& snap, const FieldParams& params, double x);

/// Quantile-matching loss of `params` over all snapshots.
double FitLoss(const std::vector<LeaderboardSnapshot>& snaps, const FieldParams& params);

/// Grid search over f_yolo (and optionally kelly_hi); passive/consensus/bot shares keep their
/// base proportions. Throws std::invalid_argument on empty or inconsistent snapshots.
FitResult FitField(const std::vector<LeaderboardSnapshot>& snaps, const FieldParams& base,
                   const FitOptions& options = {});

/// base with f_yolo replaced and the other three fractions rescaled to keep their proportions.
FieldParams WithYoloFraction(const FieldParams& base, double f_yolo);

}  // namespace susq::tourney
