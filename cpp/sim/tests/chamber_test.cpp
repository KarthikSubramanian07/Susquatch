// Chamber aggregation on toy configurations where the answer is computable by hand.
#include <gtest/gtest.h>

#include <cmath>
#include <stdexcept>

#include "susq/sim/simulator.hpp"
#include "test_util.hpp"

namespace susq::sim {
namespace {

using testing::BaseConfig;
using testing::Race;

// Margins of +-100 pts dwarf every shock, so outcomes are deterministic.
RaceSpec Sure(const std::string& id, const std::string& state, bool dem, Chamber chamber) {
  RaceSpec r = Race(id, state);
  r.mu = dem ? 100.0 : -100.0;
  r.chamber = chamber;
  return r;
}

HouseDistrictSpec District(const std::string& id, const std::string& state, double lean) {
  HouseDistrictSpec d;
  d.id = id;
  d.state = state;
  d.lean = lean;
  return d;
}

TEST(Chamber, SenateCountsSeatsNotUpPlusWins) {
  SimConfig cfg = BaseConfig(2000);
  cfg.senate.dem_seats_not_up = 48;
  cfg.races = {Sure("a", "OH", true, Chamber::kSenate), Sure("b", "TX", false, Chamber::kSenate),
               Sure("c", "ME", true, Chamber::kSenate), Sure("gov", "GA", true, Chamber::kGovernor)};
  const WorldSet w = Generate(cfg);
  Results res = Evaluate(cfg, w);
  for (uint8_t s : res.senate_dem_seats) ASSERT_EQ(s, 50);
  EXPECT_DOUBLE_EQ(res.p_senate_dem_control, 0.0);  // 50 < 51 and VP is R
  EXPECT_DOUBLE_EQ(res.senate_seat_pmf[50], 1.0);

  cfg.senate.vp_breaks_ties_dem = true;
  res = Evaluate(cfg, w);
  EXPECT_DOUBLE_EQ(res.p_senate_dem_control, 1.0);
}

TEST(Chamber, IndependentThatDoesNotCaucusIsNotCounted) {
  SimConfig cfg = BaseConfig(1000);
  cfg.senate.dem_seats_not_up = 50;
  RaceSpec ne = Sure("ne", "NE", true, Chamber::kSenate);
  ne.dem_side_is_independent = true;
  ne.dem_side_caucuses_dem = false;
  cfg.races = {ne};
  const WorldSet w = Generate(cfg);
  const Results res = Evaluate(cfg, w);
  EXPECT_DOUBLE_EQ(res.p_senate_dem_control, 0.0);
  EXPECT_DOUBLE_EQ(res.p_dem_side[0], 1.0);
  EXPECT_DOUBLE_EQ(res.p_dem_market[0], 0.0);  // the Democratic market resolves NO
  EXPECT_DOUBLE_EQ(res.p_rep_market[0], 0.0);
}

TEST(Chamber, HouseCountsUnlinkedAndLinkedDistricts) {
  SimConfig cfg = BaseConfig(5000);
  cfg.house.generic_ballot = 0.0;
  cfg.house.control_threshold = 4;
  cfg.districts = {District("A", "CA", 100), District("B", "CA", 100), District("C", "NY", 100),
                   District("D", "TX", -100), District("E", "TX", -100), District("PA-07", "PA", 0)};
  RaceSpec pa = Race("PA-07", "PA");
  pa.chamber = Chamber::kHouse;
  pa.target_p_win = 0.5;
  cfg.races = {pa};
  const WorldSet w = Generate(cfg);
  (void)Calibrate(cfg, w);
  const Results res = Evaluate(cfg, w);
  for (std::size_t d = 0; d < w.n_draws; ++d) {
    ASSERT_EQ(w.house_unlinked_dem[d], 3);
    ASSERT_EQ(res.house_dem_seats[d], 3 + (res.outcomes[d] == kDemSideWins ? 1 : 0));
    ASSERT_EQ(res.house_dem_control[d], res.outcomes[d] == kDemSideWins ? 1 : 0);
  }
  EXPECT_NEAR(res.p_house_dem_control, 0.5, 0.001);
  EXPECT_NEAR(res.house_seat_pmf[3] + res.house_seat_pmf[4], 1.0, 1e-12);
}

TEST(Chamber, GenericBallotMovesTheHouse) {
  SimConfig cfg = BaseConfig(20'000);
  cfg.house.control_threshold = 3;
  for (int j = 0; j < 5; ++j) cfg.districts.push_back(District("D" + std::to_string(j), "OH", -2.0 * j));
  cfg.house.generic_ballot = 0.0;
  const double neutral = Evaluate(cfg, Generate(cfg)).p_house_dem_control;
  cfg.house.generic_ballot = 8.0;
  const double blue = Evaluate(cfg, Generate(cfg)).p_house_dem_control;
  EXPECT_GT(blue, neutral + 0.2);
}

TEST(Chamber, NotModeledChambersAreNaN) {
  SimConfig cfg = BaseConfig(100);
  cfg.races = {Race("gov", "GA")};
  const Results res = Evaluate(cfg, Generate(cfg));
  EXPECT_TRUE(std::isnan(res.p_senate_dem_control));
  EXPECT_TRUE(std::isnan(res.p_house_dem_control));
  EXPECT_TRUE(res.senate_dem_seats.empty());
  EXPECT_TRUE(res.house_dem_seats.empty());
}

TEST(Validation, RejectsBadConfigs) {
  auto expect_invalid = [](SimConfig cfg) { EXPECT_THROW(Generate(cfg), std::invalid_argument); };
  SimConfig base = BaseConfig(100);
  base.races = {Race("a", "OH")};

  SimConfig c = base;
  c.races[0].clusters = {{"nope", 1.0}};
  expect_invalid(c);
  c = base;
  c.races[0].state = "XX";
  expect_invalid(c);
  c = base;
  c.shocks.national_nu = 2.0;
  expect_invalid(c);
  c = base;
  c.races[0].target_p_win = 1.0;
  expect_invalid(c);
  c = base;
  c.races.push_back(Race("a", "GA"));
  expect_invalid(c);
  c = base;
  c.districts = {District("X", "OH", 0)};
  c.districts[0].race_id = "missing";
  expect_invalid(c);
  c = base;
  c.n_draws = 0;
  expect_invalid(c);
}

}  // namespace
}  // namespace susq::sim
