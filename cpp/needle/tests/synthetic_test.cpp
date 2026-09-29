// SPEC §7: "needle recovers the truth on synthetic counties". Posterior means converge to the
// final margin, credible intervals cover it at close to nominal rates across many nights, and the
// model beats the naive running tally while mail-heavy early returns are still coming in.
#include <gtest/gtest.h>

#include <cmath>
#include <cstdio>

#include "susq/needle/model.hpp"
#include "synthetic.hpp"

namespace susq::needle {
namespace {

using testing::MakeSynthNight;
using testing::SynthConfig;
using testing::SynthNight;

struct Snapshot {
  double mean = 0.0;
  double sd = 0.0;
  double naive = 0.0;  // margin of everything counted so far
};

// Feeds events until the reported fraction first reaches `stop_frac` (1.0 = everything).
Snapshot RunUntil(const SynthNight& night, double stop_frac) {
  NeedleModel model(night.prior);
  for (const auto& ev : night.events) {
    EXPECT_EQ(model.Ingest(ev.result), IngestStatus::kAccepted);
    model.Recompute();
    if (model.races()[0].reported_frac >= stop_frac) break;
  }
  Snapshot s;
  s.mean = model.races()[0].margin_mean;
  s.sd = model.races()[0].margin_sd;
  double a = 0.0;
  double r = 0.0;
  for (const auto& c : model.Counties()) {
    if (std::isnan(c.reported_margin)) continue;
    // reported_margin is two-party; weight by reported votes (close enough for a baseline).
    a += c.reported_votes * (1.0 + c.reported_margin / 100.0) / 2.0;
    r += c.reported_votes * (1.0 - c.reported_margin / 100.0) / 2.0;
  }
  s.naive = a + r > 0.0 ? 100.0 * (a - r) / (a + r) : 0.0;
  return s;
}

TEST(Synthetic, PriorMatchesTheConfiguredStructure) {
  const SynthConfig cfg;
  const SynthNight night = MakeSynthNight(1, cfg);
  const NeedleModel model(night.prior);
  EXPECT_NEAR(model.RaceLevelSd("2026-SEN-XX"), cfg.race_level_sd, 1e-9);
  EXPECT_NEAR(model.races()[0].margin_mean, night.prior.races(0).prior_margin_mean(), 1e-9);
  EXPECT_NEAR(model.races()[0].margin_sd, night.prior.races(0).prior_margin_sd(), 0.05);
  EXPECT_DOUBLE_EQ(model.races()[0].reported_frac, 0.0);
}

TEST(Synthetic, ConvergesToTheTruthWhenEverythingIsCounted) {
  for (uint64_t seed = 1; seed <= 20; ++seed) {
    const SynthNight night = MakeSynthNight(seed);
    const Snapshot end = RunUntil(night, 2.0);  // never stops early
    EXPECT_NEAR(end.mean, night.true_margin, 0.05) << "seed " << seed;
    EXPECT_LT(end.sd, 0.5) << "seed " << seed;  // turnout above expectation can leave a little room
  }
}

TEST(Synthetic, CredibleIntervalsCoverAtNominalRates) {
  const int kNights = 300;
  for (double stop : {0.0, 0.25, 0.6}) {
    int in80 = 0;
    int in95 = 0;
    double sum_z2 = 0.0;
    for (int seed = 1; seed <= kNights; ++seed) {
      const SynthNight night = MakeSynthNight(1000 + static_cast<uint64_t>(seed));
      const Snapshot s = RunUntil(night, stop);
      const double zscore = (night.true_margin - s.mean) / s.sd;
      in80 += std::abs(zscore) < 1.2816 ? 1 : 0;
      in95 += std::abs(zscore) < 1.9600 ? 1 : 0;
      sum_z2 += zscore * zscore;
    }
    const double c80 = static_cast<double>(in80) / kNights;
    const double c95 = static_cast<double>(in95) / kNights;
    std::printf("coverage at reported >= %.2f: 80%%-CI %.3f, 95%%-CI %.3f, mean z^2 %.3f\n", stop, c80, c95,
                sum_z2 / kNights);
    EXPECT_NEAR(c80, 0.80, 0.07) << "reported >= " << stop;
    EXPECT_NEAR(c95, 0.95, 0.04) << "reported >= " << stop;
    EXPECT_NEAR(sum_z2 / kNights, 1.0, 0.3) << "reported >= " << stop;
  }
}

TEST(Synthetic, BeatsTheNaiveTallyWhileMailLeadsTheCount) {
  double model_err = 0.0;
  double naive_err = 0.0;
  for (uint64_t seed = 1; seed <= 60; ++seed) {
    const SynthNight night = MakeSynthNight(500 + seed);
    const Snapshot s = RunUntil(night, 0.3);
    model_err += std::abs(s.mean - night.true_margin);
    naive_err += std::abs(s.naive - night.true_margin);
  }
  EXPECT_LT(model_err, 0.5 * naive_err);
}

}  // namespace
}  // namespace susq::needle
