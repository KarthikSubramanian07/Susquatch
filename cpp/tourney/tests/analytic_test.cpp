// tourney against cases with pencil-and-paper answers (SPEC §7: "tourney matches analytic cases").
#include <gtest/gtest.h>

#include <cmath>
#include <stdexcept>

#include "susq/tourney/field.hpp"
#include "susq/tourney/tourney.hpp"

namespace susq::tourney {
namespace {

FieldParams OnlyType(uint32_t n, double passive, double kelly, double yolo, double bot) {
  FieldParams p;
  p.n_players = n;
  p.f_passive = passive;
  p.f_kelly = kelly;
  p.f_yolo = yolo;
  p.f_bot = bot;
  return p;
}

// One world, every contract pays.
Market AllPay(std::vector<double> prices) {
  Market m;
  m.n_worlds = 1;
  m.pays.assign(prices.size(), 1);
  m.prices = std::move(prices);
  return m;
}

Book AllInOn(uint32_t contract, double price) {
  Book b;
  b.positions.push_back({contract, b.start_balance / price});
  return b;
}

Book Cash() {
  Book b;
  b.cash = b.start_balance;
  return b;
}

TEST(PrizeShare, NoOpponentsTakesFirstPrize) {
  EXPECT_DOUBLE_EQ(ExpectedPrizeShare(0, 0.0, 0.0, {30000, 5000, 2500}), 30000.0);
}

TEST(PrizeShare, EveryoneTiedSplitsThePool) {
  // N others all tied with you: N+1 players share places 1..3.
  const uint32_t n = 9;
  EXPECT_NEAR(ExpectedPrizeShare(n, 0.0, 1.0, {1.0}), 1.0 / 10, 1e-12);
  EXPECT_NEAR(ExpectedPrizeShare(n, 0.0, 1.0, {1.0, 1.0, 1.0}), 3.0 / 10, 1e-12);
  EXPECT_NEAR(ExpectedPrizeShare(n, 0.0, 1.0, {30000, 5000, 2500}), 37500.0 / 10, 1e-9);
  // Only two players tied: both share 1st+2nd.
  EXPECT_NEAR(ExpectedPrizeShare(1, 0.0, 1.0, {30000, 5000, 2500}), 17500.0, 1e-9);
}

TEST(PrizeShare, MatchesBruteForceEnumeration) {
  // Enumerate every (above, tie, below) assignment of N=6 others.
  const uint32_t n = 6;
  const double a = 0.2;
  const double t = 0.3;
  const std::vector<double> prizes{30000, 5000, 2500};
  double expected = 0.0;
  int states = 1;
  for (uint32_t i = 0; i < n; ++i) states *= 3;
  for (int code = 0; code < states; ++code) {
    int c = code;
    int above = 0;
    int tie = 0;
    double prob = 1.0;
    for (uint32_t i = 0; i < n; ++i) {
      const int s = c % 3;
      c /= 3;
      above += s == 0;
      tie += s == 1;
      prob *= s == 0 ? a : (s == 1 ? t : 1.0 - a - t);
    }
    double pool = 0.0;
    for (int place = above; place <= above + tie && place < 3; ++place) pool += prizes[place];
    expected += prob * pool / (tie + 1);
  }
  EXPECT_NEAR(ExpectedPrizeShare(n, a, t, prizes), expected, 1e-9);
}

TEST(Analytic, AllYoloOnOneContract) {
  // N YOLO players all-in on the only eligible contract (price 0.1 -> 10x), which pays.
  const uint32_t n = 50;
  const FieldModel field(AllPay({0.1}), OnlyType(n, 0, 0, 1, 0));
  // You match them exactly: an (N+1)-way tie.
  const BookEvaluation tied = EvaluateBook(field, AllInOn(0, 0.1));
  EXPECT_NEAR(tied.multiple[0], 10.0, 1e-12);
  EXPECT_NEAR(tied.p_first, 1.0 / (n + 1), 1e-12);
  EXPECT_NEAR(tied.p_top3, 3.0 / (n + 1), 1e-12);
  EXPECT_NEAR(tied.expected_prize, 37500.0 / (n + 1), 1e-9);
  // Anything else loses to all of them.
  EXPECT_NEAR(EvaluateBook(field, Cash()).p_first, 0.0, 1e-15);
  // Beat them by a hair and you win outright.
  Book better = AllInOn(0, 0.1);
  better.cash = 1.0;
  EXPECT_NEAR(EvaluateBook(field, better).p_first, 1.0, 1e-12);
  // The field maximum is exactly 10.
  EXPECT_NEAR(ThresholdQuantiles(field, {0.01, 0.5, 0.99})[1], 10.0, 1e-12);
}

TEST(Analytic, EverybodyPassiveTies) {
  const uint32_t n = 199;
  const FieldModel field(AllPay({0.2}), OnlyType(n, 1, 0, 0, 0));
  const BookEvaluation e = EvaluateBook(field, Cash());
  EXPECT_NEAR(e.p_first, 1.0 / 200, 1e-12);
  EXPECT_NEAR(e.p_top3, 3.0 / 200, 1e-12);
  EXPECT_NEAR(e.expected_prize, 37500.0 / 200, 1e-9);
}

TEST(Analytic, TwoLongshotsClosedForm) {
  // YOLO weights proportional to price: 0.05 (20x) gets 1/3, 0.10 (10x) gets 2/3. Both pay.
  // At M = 15 you lose only to players on the 20x contract: P(1st) = (2/3)^N.
  const uint32_t n = 12;
  const FieldModel field(AllPay({0.05, 0.10}), OnlyType(n, 0, 0, 1, 0));
  EXPECT_NEAR(field.yolo_weights()[0], 1.0 / 3.0, 1e-12);
  Book b = Cash();
  b.cash = 15.0 * b.start_balance;
  const BookEvaluation e = EvaluateBook(field, b);
  EXPECT_NEAR(e.p_first, std::pow(2.0 / 3.0, n), 1e-12);
  double top3 = 0.0;
  for (int k = 0; k <= 2; ++k) {
    const double choose = k == 0 ? 1.0 : (k == 1 ? n : n * (n - 1) / 2.0);
    top3 += choose * std::pow(1.0 / 3.0, k) * std::pow(2.0 / 3.0, n - k);
  }
  EXPECT_NEAR(e.p_top3, top3, 1e-12);
}

TEST(Analytic, ConsensusRangeIsLinear) {
  // Kelly players ~ U[0.8, 1.3]; at M = 1.05 each is above with probability 1/2.
  const uint32_t n = 5;
  const FieldModel field(AllPay({0.3}), OnlyType(n, 0, 1, 0, 0));
  Book b = Cash();
  b.cash = 1.05 * b.start_balance;
  EXPECT_NEAR(EvaluateBook(field, b).p_first, std::pow(0.5, n), 1e-12);
  EXPECT_NEAR(ThresholdCdf(field, {1.05})[0], std::pow(0.5, n), 1e-12);
}

TEST(Analytic, BotRollsDistribution) {
  FieldParams p = OnlyType(1, 0, 0, 0, 1);
  p.bot_rolls = 2;
  p.bot_roll_multiple = 3.0;
  p.bot_roll_success = 0.5;
  p.bot_fail_keep = 0.0;
  const auto pmf = BotPmf(p);
  ASSERT_EQ(pmf.size(), 2U);  // {0: 0.75, 9: 0.25}
  EXPECT_DOUBLE_EQ(pmf[0].first, 0.0);
  EXPECT_DOUBLE_EQ(pmf[0].second, 0.75);
  EXPECT_DOUBLE_EQ(pmf[1].first, 9.0);
  EXPECT_DOUBLE_EQ(pmf[1].second, 0.25);
}

TEST(Analytic, WorldsWeightTheThreshold) {
  // Contract pays in world 0 only; all players YOLO on it: T = 10 in world 0, 0 in world 1.
  Market m;
  m.prices = {0.1};
  m.n_worlds = 2;
  m.pays = {1, 0};
  m.world_weights = {0.3, 0.7};
  const FieldModel field(m, OnlyType(10, 0, 0, 1, 0));
  const auto q = ThresholdQuantiles(field, {0.5, 0.8});
  EXPECT_DOUBLE_EQ(q[0], 0.0);
  EXPECT_DOUBLE_EQ(q[1], 10.0);
  // Holding cash (1x) beats everyone in world 1 only.
  EXPECT_NEAR(EvaluateBook(field, Cash()).p_first, 0.7, 1e-12);
}

TEST(Analytic, BiggerFieldsRaiseTheBar) {
  std::vector<double> prices;
  for (int j = 1; j <= 40; ++j) prices.push_back(0.005 * j);
  Market m = AllPay(prices);
  FieldParams p = OnlyType(500, 0.6, 0.25, 0.1, 0.05);
  const double t500 = ThresholdQuantiles(FieldModel(m, p), {0.5})[0];
  p.n_players = 10000;
  const double t10k = ThresholdQuantiles(FieldModel(m, p), {0.5})[0];
  EXPECT_GE(t10k, t500);
  EXPECT_GE(t10k, 100.0);  // someone almost surely holds the 0.5-cent winner
}

TEST(Analytic, ThreadCountDoesNotChangeResults) {
  std::vector<double> prices{0.02, 0.05, 0.1, 0.2, 0.4};
  Market m;
  m.prices = prices;
  m.n_worlds = 1000;
  for (std::size_t j = 0; j < prices.size(); ++j) {
    for (std::size_t w = 0; w < m.n_worlds; ++w) m.pays.push_back(((w * 7 + j * 13) % 10) < prices[j] * 20);
  }
  const FieldModel field(m, OnlyType(800, 0.5, 0.3, 0.15, 0.05));
  const auto one = ThresholdCdf(field, {1.0, 5.0, 12.0, 30.0}, 1);
  const auto many = ThresholdCdf(field, {1.0, 5.0, 12.0, 30.0}, 7);
  EXPECT_EQ(one, many);
  const Book b = AllInOn(1, 0.05);
  EXPECT_EQ(EvaluateBook(field, b, {}, 1).p_first, EvaluateBook(field, b, {}, 5).p_first);
}

TEST(Validation, RejectsBadInputs) {
  EXPECT_THROW(FieldModel(AllPay({0.1}), OnlyType(10, 0.5, 0.2, 0.2, 0.2)), std::invalid_argument);
  EXPECT_THROW(FieldModel(AllPay({1.0}), OnlyType(10, 1, 0, 0, 0)), std::invalid_argument);
  EXPECT_THROW(FieldModel(AllPay({0.9}), OnlyType(10, 0.5, 0, 0.5, 0)), std::invalid_argument);
  Market bad = AllPay({0.1});
  bad.pays.push_back(1);
  EXPECT_THROW(FieldModel(bad, OnlyType(10, 1, 0, 0, 0)), std::invalid_argument);
  const FieldModel ok(AllPay({0.1}), OnlyType(10, 1, 0, 0, 0));
  Book b;
  b.positions.push_back({3, 1.0});
  EXPECT_THROW(EvaluateBook(ok, b), std::invalid_argument);
}

}  // namespace
}  // namespace susq::tourney
