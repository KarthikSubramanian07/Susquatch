#include <gtest/gtest.h>

#include <algorithm>
#include <functional>
#include <random>
#include <stdexcept>

#include "susq/tourney/fit.hpp"

namespace susq::tourney {
namespace {

// Samples a leaderboard from the field model with known parameters.
LeaderboardSnapshot SyntheticSnapshot(const FieldParams& truth, std::size_t top_k, uint64_t seed) {
  LeaderboardSnapshot snap;
  snap.n_players = truth.n_players;
  snap.bots_active = true;
  std::mt19937_64 rng(seed);
  std::uniform_real_distribution<double> u(0.0, 1.0);
  for (int j = 0; j < 30; ++j) {
    const double price = 0.01 + 0.01 * j;
    snap.entry_prices.push_back(price);
    // Resolved world: roughly price-probability of paying.
    snap.contract_values.push_back(u(rng) < price * 1.5 ? 1.0 / price : 0.0);
  }
  const std::vector<double> w = YoloWeights(snap.entry_prices, truth);
  std::discrete_distribution<std::size_t> pick(w.begin(), w.end());
  const auto bots = BotPmf(truth);
  std::vector<double> players;
  for (uint32_t i = 0; i < truth.n_players; ++i) {
    const double t = u(rng);
    if (t < truth.f_passive) {
      players.push_back(1.0);
    } else if (t < truth.f_passive + truth.f_kelly) {
      players.push_back(truth.kelly_lo + (truth.kelly_hi - truth.kelly_lo) * u(rng));
    } else if (t < truth.f_passive + truth.f_kelly + truth.f_yolo) {
      players.push_back(snap.contract_values[pick(rng)]);
    } else {
      double c = u(rng);
      double v = bots.back().first;
      for (const auto& [value, prob] : bots) {
        if ((c -= prob) < 0) {
          v = value;
          break;
        }
      }
      players.push_back(v);
    }
  }
  std::sort(players.begin(), players.end(), std::greater<>());
  snap.top_multiples.assign(players.begin(), players.begin() + static_cast<std::ptrdiff_t>(top_k));
  return snap;
}

TEST(Fit, RecoversYoloFraction) {
  FieldParams truth;
  truth.n_players = 3000;
  truth.f_yolo = 0.12;
  truth.f_passive = 0.5;
  truth.f_kelly = 0.33;
  truth.f_bot = 0.05;
  std::vector<LeaderboardSnapshot> snaps;
  for (uint64_t s = 1; s <= 4; ++s) snaps.push_back(SyntheticSnapshot(truth, 60, s));

  FieldParams base = truth;
  base = WithYoloFraction(base, 0.3);  // start from the wrong mix
  const FitResult fit = FitField(snaps, base);
  EXPECT_NEAR(fit.params.f_yolo, 0.12, 0.04);
  EXPECT_NEAR(fit.params.f_passive + fit.params.f_kelly + fit.params.f_yolo + fit.params.f_bot, 1.0, 1e-9);
  // Non-YOLO types keep their base proportions.
  EXPECT_NEAR(fit.params.f_kelly / fit.params.f_passive, truth.f_kelly / truth.f_passive, 1e-9);
  ASSERT_EQ(fit.loss_by_f_yolo.size(), fit.f_yolo_grid.size());
  EXPECT_DOUBLE_EQ(*std::min_element(fit.loss_by_f_yolo.begin(), fit.loss_by_f_yolo.end()), fit.loss);
}

TEST(Fit, FitsConsensusSpreadWhenAsked) {
  FieldParams truth;
  truth.n_players = 2000;
  truth.f_passive = 0.3;
  truth.f_kelly = 0.7;
  truth.f_yolo = 0.0;
  truth.f_bot = 0.0;
  truth.kelly_hi = 1.6;
  const std::vector<LeaderboardSnapshot> snaps{SyntheticSnapshot(truth, 100, 7)};
  FitOptions opts;
  opts.f_yolo_grid = {0.0, 0.05};
  opts.kelly_hi_grid = {1.1, 1.3, 1.6, 2.0};
  const FitResult fit = FitField(snaps, truth, opts);
  EXPECT_DOUBLE_EQ(fit.params.kelly_hi, 1.6);
  EXPECT_DOUBLE_EQ(fit.params.f_yolo, 0.0);
}

TEST(Fit, MidSurvivalCountsTiesAsHalf) {
  LeaderboardSnapshot snap;
  snap.n_players = 10;
  snap.top_multiples = {1.0};
  FieldParams p;
  p.f_passive = 1.0;
  p.f_kelly = p.f_yolo = p.f_bot = 0.0;
  EXPECT_DOUBLE_EQ(SnapshotSurvival(snap, p, 1.0), 0.5);
  EXPECT_DOUBLE_EQ(SnapshotSurvival(snap, p, 0.9), 1.0);
  EXPECT_DOUBLE_EQ(SnapshotSurvival(snap, p, 1.1), 0.0);
}

TEST(Fit, RejectsBadSnapshots) {
  FieldParams base;
  EXPECT_THROW(FitField({}, base), std::invalid_argument);
  LeaderboardSnapshot s;
  s.n_players = 5;
  EXPECT_THROW(FitField({s}, base), std::invalid_argument);  // no top multiples
  s.top_multiples = {2.0};
  s.entry_prices = {0.1};
  EXPECT_THROW(FitField({s}, base), std::invalid_argument);  // size mismatch
}

}  // namespace
}  // namespace susq::tourney
