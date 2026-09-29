#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <stdexcept>

#include "susq/needle/model.hpp"
#include "susq/needle/service.hpp"
#include "synthetic.hpp"

namespace susq::needle {
namespace {

using testing::AddVoteType;
using testing::TestHeader;

susq::v1::RacePrior* AddRace(susq::v1::NeedlePrior* p, const std::string& id, double mean, double sd,
                             double beta = 1.0) {
  auto* r = p->add_races();
  r->set_race_id(id);
  r->set_state(id.substr(id.size() - 2));
  r->set_prior_margin_mean(mean);
  r->set_prior_margin_sd(sd);
  r->set_national_beta(beta);
  return r;
}

susq::v1::CountyPrior* AddCounty(susq::v1::NeedlePrior* p, const std::string& race, const std::string& fips,
                                 double expected, double margin, double sd = 4.0) {
  auto* c = p->add_counties();
  c->set_race_id(race);
  c->set_county_fips(fips);
  c->set_expected_total_votes(expected);
  c->set_expected_total_sd_frac(0.0);
  c->set_prior_margin(margin);
  c->set_prior_margin_sd(sd);
  return c;
}

susq::v1::CountyResult Result(const std::string& race, const std::string& fips, susq::v1::VoteType type,
                              uint64_t dem, uint64_t rep, const std::string& batch, const std::string& source = "sos") {
  susq::v1::CountyResult r;
  r.set_race_id(race);
  r.set_county_fips(fips);
  r.set_vote_type(type);
  r.set_source(source);
  auto* d = r.add_votes();
  d->set_party("DEM");
  d->set_votes(dem);
  auto* rp = r.add_votes();
  rp->set_party("REP");
  rp->set_votes(rep);
  r.set_total_votes(dem + rep);
  r.set_batch_id(batch);
  return r;
}

const RaceEstimate& Find(const NeedleModel& m, const std::string& id) {
  return *std::find_if(m.races().begin(), m.races().end(), [&](const RaceEstimate& e) { return e.race_id == id; });
}

// Race A (one precisely measured county) informs N; race B has no counties and moves via beta.
TEST(NationalShift, PosteriorMatchesTheGaussianUpdate) {
  NeedleParams params;
  params.type_sd = 0.01;
  params.within_type_sd = 0.01;
  params.obs_var_floor = 1e-6;
  const double v0 = 2.75 * 2.75;
  susq::v1::NeedlePrior prior;
  *prior.mutable_header() = TestHeader();
  prior.set_national_shift_prior_sd(2.75);
  AddRace(&prior, "2026-SEN-OH", 0.0, std::sqrt(v0 + 4.0));
  AddRace(&prior, "2026-SEN-AZ", 1.0, 5.0, 0.5);
  AddCounty(&prior, "2026-SEN-OH", "39001", 100000, 0.0, 0.001);
  NeedleModel model(prior, params);
  const double delta_var = std::pow(model.RaceLevelSd("2026-SEN-OH"), 2);
  EXPECT_NEAR(delta_var, 4.0, 1e-3);

  // OH comes in fully counted at D+6 against a prior of 0.
  model.Ingest(Result("2026-SEN-OH", "39001", susq::v1::VOTE_TYPE_MIXED, 53000, 47000, "b1"));
  model.Recompute();
  const double expected_mean = 6.0 * v0 / (v0 + delta_var);
  const double expected_var = v0 * delta_var / (v0 + delta_var);
  EXPECT_NEAR(model.national().mean, expected_mean, 1e-3);
  EXPECT_NEAR(model.national().sd, std::sqrt(expected_var), 1e-3);
  EXPECT_EQ(model.national().n_races_informing, 1U);
  EXPECT_NEAR(Find(model, "2026-SEN-OH").margin_mean, 6.0, 1e-6);

  // AZ has no data: its mean moves by beta * E[N]; its variance shrinks accordingly.
  const RaceEstimate& az = Find(model, "2026-SEN-AZ");
  const double az_delta = std::pow(model.RaceLevelSd("2026-SEN-AZ"), 2);
  EXPECT_NEAR(az.margin_mean, 1.0 + 0.5 * expected_mean, 1e-3);
  EXPECT_NEAR(az.margin_sd, std::sqrt(0.25 * expected_var + az_delta), 1e-3);
  EXPECT_NEAR(az.reported_frac, 0.0, 1e-12);
}

TEST(NationalShift, NoDataMeansPrior) {
  susq::v1::NeedlePrior prior;
  *prior.mutable_header() = TestHeader();
  prior.set_national_shift_prior_sd(2.75);
  AddRace(&prior, "2026-SEN-OH", 1.5, 5.0);
  const NeedleModel model(prior);
  EXPECT_DOUBLE_EQ(model.national().mean, 0.0);
  EXPECT_DOUBLE_EQ(model.national().sd, 2.75);
  EXPECT_NEAR(model.races()[0].margin_sd, 5.0, 1e-9);
  EXPECT_NEAR(model.races()[0].p_dem, 0.5 * std::erfc(-1.5 / 5.0 / std::sqrt(2.0)), 1e-12);
  EXPECT_NEAR(model.races()[0].p_dem + model.races()[0].p_rep, 1.0, 1e-12);
}

class Ingestion : public ::testing::Test {
 protected:
  void SetUp() override {
    *prior_.mutable_header() = TestHeader();
    prior_.set_national_shift_prior_sd(2.75);
    AddRace(&prior_, "2026-SEN-OH", 1.0, 5.0);
    auto* c = AddCounty(&prior_, "2026-SEN-OH", "39049", 1000, 20.0);
    AddVoteType(c, susq::v1::VOTE_TYPE_MAIL, 0.5, 20.0, 0.0);
    AddVoteType(c, susq::v1::VOTE_TYPE_ELECTION_DAY, 0.5, -20.0, 1.0);
    AddCounty(&prior_, "2026-SEN-OH", "39035", 1000, -10.0);
  }
  susq::v1::NeedlePrior prior_;
};

TEST_F(Ingestion, DedupesByBatchId) {
  NeedleModel m(prior_);
  const auto r = Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 300, 150, "b1");
  EXPECT_EQ(m.Ingest(r), IngestStatus::kAccepted);
  m.Recompute();
  const double before = m.races()[0].margin_mean;
  EXPECT_EQ(m.Ingest(r), IngestStatus::kDuplicateBatch);
  m.Recompute();
  EXPECT_DOUBLE_EQ(m.races()[0].margin_mean, before);
  EXPECT_EQ(m.races()[0].n_batches, 1U);
  EXPECT_EQ(m.counters().duplicate_batch, 1U);
  // Same totals under a new batch id change nothing either.
  auto again = r;
  again.set_batch_id("b2");
  EXPECT_EQ(m.Ingest(again), IngestStatus::kUnchanged);
}

TEST_F(Ingestion, IgnoresRegressionsUnlessCorrections) {
  NeedleModel m(prior_);
  EXPECT_EQ(m.Ingest(Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 300, 150, "b1")), IngestStatus::kAccepted);
  const auto lower = Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 250, 150, "b2");
  EXPECT_EQ(m.Ingest(lower), IngestStatus::kRegression);
  m.Recompute();
  EXPECT_NEAR(m.Counties()[0].reported_votes, 450.0, 1e-9);
  auto corrected = lower;
  corrected.set_batch_id("b3");
  EXPECT_EQ(m.Ingest(corrected, /*is_correction=*/true), IngestStatus::kAccepted);
  m.Recompute();
  EXPECT_NEAR(m.Counties()[0].reported_votes, 400.0, 1e-9);
}

TEST_F(Ingestion, TakesTheMostCompleteSource) {
  NeedleModel m(prior_);
  m.Ingest(Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 300, 150, "a1", "clarity:OH"));
  m.Ingest(Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 200, 100, "s1", "oh_sos"));
  m.Recompute();
  EXPECT_NEAR(m.Counties()[0].reported_votes, 450.0, 1e-9);
}

TEST_F(Ingestion, RejectsMalformedAndUnknown) {
  NeedleModel m(prior_);
  auto no_votes = Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 1, 1, "x1");
  no_votes.clear_votes();
  EXPECT_EQ(m.Ingest(no_votes), IngestStatus::kInvalid);
  auto short_total = Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 300, 150, "x2");
  short_total.set_total_votes(100);
  EXPECT_EQ(m.Ingest(short_total), IngestStatus::kInvalid);
  auto huge = Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 1, 1, "x3");
  huge.set_total_votes(UINT64_MAX);
  EXPECT_EQ(m.Ingest(huge), IngestStatus::kInvalid);
  auto future_type = Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 1, 1, "x4");
  future_type.set_vote_type(static_cast<susq::v1::VoteType>(42));
  EXPECT_EQ(m.Ingest(future_type), IngestStatus::kInvalid);
  EXPECT_EQ(m.Ingest(Result("2026-SEN-ZZ", "1", susq::v1::VOTE_TYPE_MAIL, 1, 1, "x5")), IngestStatus::kUnknownRace);
  EXPECT_EQ(m.counters().invalid, 4U);
  EXPECT_EQ(m.counters().unknown_race, 1U);
  // Nothing leaked into the model.
  m.Recompute();
  EXPECT_DOUBLE_EQ(m.races()[0].reported_frac, 0.0);
}

TEST_F(Ingestion, UnmodeledCountiesAreCountedNotModeled) {
  NeedleModel m(prior_);
  EXPECT_EQ(m.Ingest(Result("2026-SEN-OH", "39999", susq::v1::VOTE_TYPE_MIXED, 700, 300, "u1")),
            IngestStatus::kUnmodeledCounty);
  m.Recompute();
  EXPECT_NEAR(m.races()[0].reported_frac, 1000.0 / 3000.0, 1e-9);
  EXPECT_EQ(m.national().n_races_informing, 0U);
}

TEST_F(Ingestion, MixedTalliesFillTypesInReportingOrder) {
  NeedleModel m(prior_);
  // 500 votes at exactly the mail prior (20 + 20 = D+40): mail has reported, E-day has not.
  m.Ingest(Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MIXED, 350, 150, "m1"));
  m.Recompute();
  const CountyEstimate c = m.Counties()[0];
  EXPECT_NEAR(c.expected_remaining_votes, 500.0, 1e-9);
  EXPECT_LT(c.remaining_margin, 10.0);  // remaining votes are Election Day (prior 20 - 20 = 0)
  EXPECT_GT(c.remaining_margin, -10.0);
}

TEST_F(Ingestion, TypedResultsUpdateOnlyTheirType) {
  NeedleModel m(prior_);
  m.Ingest(Result("2026-SEN-OH", "39049", susq::v1::VOTE_TYPE_MAIL, 350, 150, "t1"));  // all mail, D+40
  m.Recompute();
  const CountyEstimate c = m.Counties()[0];
  EXPECT_NEAR(c.expected_remaining_votes, 500.0, 1e-9);
  EXPECT_NEAR(c.reported_margin, 40.0, 1e-9);
  // Aligned priors: county 16, mail 36, E-day -4. Mail came in 4 pts above its prior, so the
  // remaining E-day votes move up from -4 by a shrunk share of that surprise.
  EXPECT_GT(c.remaining_margin, -4.0);
  EXPECT_LT(c.remaining_margin, 0.0);
}

TEST(IndependentMajor, LeavesMassForNeither) {
  susq::v1::NeedlePrior prior;
  *prior.mutable_header() = TestHeader();
  prior.set_national_shift_prior_sd(2.75);
  auto* ne = AddRace(&prior, "2026-SEN-NE", -1.0, 5.0);
  ne->set_independent_major(true);
  AddCounty(&prior, "2026-SEN-NE", "31055", 1000, -1.0);
  NeedleModel m(prior);
  susq::v1::CountyResult r;
  r.set_race_id("2026-SEN-NE");
  r.set_county_fips("31055");
  r.set_vote_type(susq::v1::VOTE_TYPE_MIXED);
  auto* i = r.add_votes();
  i->set_party("IND");
  i->set_votes(600);
  auto* rep = r.add_votes();
  rep->set_party("REP");
  rep->set_votes(400);
  EXPECT_EQ(m.Ingest(r), IngestStatus::kAccepted);
  m.Recompute();
  const RaceEstimate& e = m.races()[0];
  EXPECT_NEAR(e.margin_mean, 20.0, 1e-6);
  EXPECT_LT(e.p_rep, 1e-6);
  EXPECT_DOUBLE_EQ(e.p_dem, 0.0);  // the independent, not the Democrat, is winning
  EXPECT_LT(e.p_dem + e.p_rep, 1.0);
}

TEST(Prior, RejectsInconsistentPriors) {
  susq::v1::NeedlePrior prior;
  *prior.mutable_header() = TestHeader();
  AddRace(&prior, "2026-SEN-OH", 0.0, 5.0);
  AddRace(&prior, "2026-SEN-OH", 0.0, 5.0);
  EXPECT_THROW(NeedleModel{prior}, std::invalid_argument);
  prior.mutable_races()->RemoveLast();
  AddCounty(&prior, "2026-SEN-XX", "1", 10, 0);
  EXPECT_THROW(NeedleModel{prior}, std::invalid_argument);
  prior.mutable_counties()->RemoveLast();
  prior.mutable_races(0)->set_prior_margin_mean(std::nan(""));
  EXPECT_THROW(NeedleModel{prior}, std::invalid_argument);
}

TEST(Prior, LoadsTheGoldenFixture) {
  const auto prior = LoadPrior(std::string(SUSQ_FIXTURES_DIR) + "/needle_prior.json");
  NeedleModel m(prior);
  ASSERT_EQ(m.races().size(), 1U);
  EXPECT_EQ(m.races()[0].race_id, "2026-SEN-OH-S");
  EXPECT_NEAR(m.races()[0].margin_mean, 1.5, 1e-9);
  EXPECT_THROW(ParsePrior("{\"races\": []}"), std::invalid_argument);  // no header
  EXPECT_THROW(ParsePrior("not json"), std::invalid_argument);
}

TEST(Performance, RecomputeIsFastForThreeThousandCounties) {
  susq::v1::NeedlePrior prior;
  *prior.mutable_header() = TestHeader();
  prior.set_national_shift_prior_sd(2.75);
  const int n_races = 60;
  for (int r = 0; r < n_races; ++r) AddRace(&prior, "R" + std::to_string(r) + "-XX", 0.0, 5.0);
  for (int c = 0; c < 3000; ++c) {
    auto* cp = AddCounty(&prior, "R" + std::to_string(c % n_races) + "-XX", std::to_string(10000 + c), 50000, 0.0);
    AddVoteType(cp, susq::v1::VOTE_TYPE_MAIL, 0.4, 5.0, 0.0);
    AddVoteType(cp, susq::v1::VOTE_TYPE_EARLY_IN_PERSON, 0.2, 0.0, 0.3);
    AddVoteType(cp, susq::v1::VOTE_TYPE_ELECTION_DAY, 0.4, -5.0, 0.6);
  }
  NeedleModel m(prior);
  for (int c = 0; c < 3000; ++c) {
    m.Ingest(Result("R" + std::to_string(c % n_races) + "-XX", std::to_string(10000 + c), susq::v1::VOTE_TYPE_MAIL,
                    6000, 5000, "p" + std::to_string(c)));
  }
  std::vector<double> ms;
  for (int i = 0; i < 15; ++i) {
    const auto t0 = std::chrono::steady_clock::now();
    m.Recompute();
    ms.push_back(std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count());
  }
  std::sort(ms.begin(), ms.end());
  std::printf("needle recompute, 3000 counties: median %.3f ms\n", ms[ms.size() / 2]);
  EXPECT_LT(ms[ms.size() / 2], 10.0);
}

}  // namespace
}  // namespace susq::needle
