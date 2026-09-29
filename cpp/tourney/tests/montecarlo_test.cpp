// Cross-checks the closed forms against a literal simulation of players, ties and prize pooling.
#include <gtest/gtest.h>

#include <algorithm>
#include <random>

#include "susq/tourney/field.hpp"
#include "susq/tourney/tourney.hpp"

namespace susq::tourney {
namespace {

// Draws one field player's multiple in world w, straight from the type definitions.
double DrawPlayer(const FieldModel& field, std::size_t w, std::mt19937_64& rng) {
  const FieldParams& p = field.params();
  std::uniform_real_distribution<double> u(0.0, 1.0);
  const double type = u(rng);
  if (type < p.f_passive) return 1.0;
  if (type < p.f_passive + p.f_kelly) return p.kelly_lo + (p.kelly_hi - p.kelly_lo) * u(rng);
  if (type < p.f_passive + p.f_kelly + p.f_yolo) {
    std::discrete_distribution<std::size_t> pick(field.yolo_weights().begin(), field.yolo_weights().end());
    const std::size_t j = pick(rng);
    return field.market().pays[j * field.market().n_worlds + w] ? 1.0 / field.market().prices[j] : 0.0;
  }
  double m = 1.0;
  for (int r = 0; r < p.bot_rolls; ++r) m *= u(rng) < p.bot_roll_success ? p.bot_roll_multiple : p.bot_fail_keep;
  return m;
}

TEST(MonteCarlo, ClosedFormMatchesSimulatedTournaments) {
  Market market;
  market.prices = {0.04, 0.08, 0.2, 0.5};
  market.n_worlds = 4;
  //            w0 w1 w2 w3
  market.pays = {1, 0, 0, 1,   // 25x
                 0, 1, 0, 1,   // 12.5x
                 1, 1, 0, 0,   // 5x
                 0, 0, 1, 1};  // 2x (ineligible for YOLO at max price 0.25)
  FieldParams params;
  params.n_players = 20;
  params.f_passive = 0.4;
  params.f_kelly = 0.3;
  params.f_yolo = 0.2;
  params.f_bot = 0.1;
  params.bot_rolls = 2;
  params.bot_roll_multiple = 2.5;  // bots end at 0 or 6.25
  params.bot_roll_success = 0.5;
  const FieldModel field(market, params);

  Book book;  // 5x in worlds 0-1 via the 0.2 contract, else 0; a tie with YOLOs there
  book.positions.push_back({2, book.start_balance / 0.2});
  const Prizes prizes;
  const BookEvaluation exact = EvaluateBook(field, book, prizes);

  std::mt19937_64 rng(12345);
  const int sims_per_world = 60000;
  double p_first = 0.0;
  double p_top3 = 0.0;
  double prize = 0.0;
  std::vector<double> others(params.n_players);
  for (std::size_t w = 0; w < market.n_worlds; ++w) {
    for (int s = 0; s < sims_per_world; ++s) {
      int above = 0;
      int tied = 0;
      for (auto& x : others) {
        x = DrawPlayer(field, w, rng);
        if (SameMultiple(x, exact.multiple[w])) {
          ++tied;
        } else if (x > exact.multiple[w]) {
          ++above;
        }
      }
      double pool = 0.0;
      double places_top3 = 0.0;
      for (int place = above; place <= above + tied; ++place) {
        if (place < 3) {
          pool += prizes.amounts[static_cast<std::size_t>(place)];
          places_top3 += 1.0;
        }
      }
      const double share = 1.0 / (tied + 1);
      p_first += (above == 0 ? share : 0.0) * field.world_weight(w);
      p_top3 += places_top3 * share * field.world_weight(w);
      prize += pool * share * field.world_weight(w);
    }
  }
  p_first /= sims_per_world;
  p_top3 /= sims_per_world;
  prize /= sims_per_world;
  EXPECT_NEAR(exact.p_first, p_first, 0.004);
  EXPECT_NEAR(exact.p_top3, p_top3, 0.006);
  EXPECT_NEAR(exact.expected_prize, prize, 150.0);
  EXPECT_GT(exact.p_first, 0.01);  // the test is not vacuous
}

TEST(MonteCarlo, ThresholdCdfMatchesSimulatedMaximum) {
  Market market;
  market.prices = {0.02, 0.05, 0.1, 0.25};
  market.n_worlds = 3;
  market.pays = {1, 0, 0, 0, 1, 0, 1, 1, 0, 0, 0, 1};
  FieldParams params;
  params.n_players = 150;
  const FieldModel field(market, params);
  const std::vector<double> grid{1.0, 1.2, 2.0, 6.25, 10.0, 20.0, 50.0};
  const auto cdf = ThresholdCdf(field, grid);

  std::mt19937_64 rng(99);
  const int sims = 20000;
  std::vector<double> hits(grid.size(), 0.0);
  for (std::size_t w = 0; w < market.n_worlds; ++w) {
    for (int s = 0; s < sims; ++s) {
      double t = 0.0;
      for (uint32_t i = 0; i < params.n_players; ++i) t = std::max(t, DrawPlayer(field, w, rng));
      for (std::size_t g = 0; g < grid.size(); ++g) hits[g] += (t <= grid[g] + 1e-12) * field.world_weight(w);
    }
  }
  for (std::size_t g = 0; g < grid.size(); ++g) EXPECT_NEAR(cdf[g], hits[g] / sims, 0.012) << grid[g];
}

}  // namespace
}  // namespace susq::tourney
