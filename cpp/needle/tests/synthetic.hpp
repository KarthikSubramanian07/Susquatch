// Synthetic election night for one state, drawn from the needle's own generative model so that
// posterior calibration can be checked exactly: known final margin, cumulative county results
// in a realistic mail-first / early / Election-Day-last order.
#pragma once

#include <algorithm>
#include <cmath>
#include <random>
#include <string>
#include <vector>

#include "susq/contracts.hpp"
#include "susq/needle/model.hpp"

namespace susq::needle::testing {

struct SynthConfig {
  int n_counties = 40;
  int chunks_per_type = 4;           // equal-size reporting batches per (county, vote type)
  double national_sd = 2.75;
  double race_level_sd = 2.0;        // delta_r
  double county_sd = 4.0;            // eta_c (county prior_margin_sd)
  double turnout_sd_frac = 0.05;
  double two_party_share = 0.97;
  NeedleParams params;               // type_sd / within_type_sd used for zeta and batch noise
};

struct SynthEvent {
  susq::v1::CountyResult result;     // cumulative totals after this batch
};

struct SynthNight {
  susq::v1::NeedlePrior prior;
  double true_margin = 0.0;          // final two-party margin of the race
  double true_national = 0.0;
  std::vector<SynthEvent> events;    // in reporting order
};

inline void AddVoteType(susq::v1::CountyPrior* c, susq::v1::VoteType t, double share, double offset, double order) {
  auto* v = c->add_vote_types();
  v->set_vote_type(t);
  v->set_share_of_total(share);
  v->set_margin_offset(offset);
  v->set_typical_report_order(order);
}

inline susq::v1::Header TestHeader(uint64_t seq = 1) {
  return susq::contracts::MakeHeader("test", "0000000", seq, 1791475200000000000LL);
}

/// Builds one race "2026-SEN-XX" in state XX with the configured structure.
inline SynthNight MakeSynthNight(uint64_t seed, const SynthConfig& cfg = {}) {
  std::mt19937_64 rng(seed);
  std::normal_distribution<double> z(0.0, 1.0);
  std::uniform_real_distribution<double> u(0.0, 1.0);
  const std::string race_id = "2026-SEN-XX";

  struct TypeDef {
    susq::v1::VoteType type;
    double share, offset, order;
  };
  // Offsets average to ~0 by share: mail leans D, Election Day leans R.
  const TypeDef types[] = {{susq::v1::VOTE_TYPE_MAIL, 0.35, 9.0, 0.0},
                           {susq::v1::VOTE_TYPE_EARLY_IN_PERSON, 0.20, 2.0, 0.3},
                           {susq::v1::VOTE_TYPE_ELECTION_DAY, 0.45, -(0.35 * 9.0 + 0.20 * 2.0) / 0.45, 0.6}};

  SynthNight night;
  susq::v1::NeedlePrior& prior = night.prior;
  *prior.mutable_header() = TestHeader();
  prior.set_national_shift_prior_sd(cfg.national_sd);

  std::vector<double> expected(static_cast<std::size_t>(cfg.n_counties));
  std::vector<double> county_prior(expected.size());
  double total_e = 0.0;
  double weighted = 0.0;
  double idio = 0.0;
  for (std::size_t c = 0; c < expected.size(); ++c) {
    expected[c] = std::round(5000.0 + 195000.0 * std::pow(u(rng), 2));
    county_prior[c] = 15.0 * z(rng);
    total_e += expected[c];
    weighted += expected[c] * county_prior[c];
  }
  const double race_prior = weighted / total_e;  // county priors already aggregate to the race prior
  double share_sq = 0.0;
  for (const auto& t : types) share_sq += t.share * t.share;
  for (std::size_t c = 0; c < expected.size(); ++c) {
    const double w = expected[c] / total_e;
    idio += w * w * (cfg.county_sd * cfg.county_sd + cfg.params.type_sd * cfg.params.type_sd * share_sq);
    idio += std::pow((county_prior[c] - race_prior) * cfg.turnout_sd_frac * w, 2);
  }
  auto* rp = prior.add_races();
  rp->set_race_id(race_id);
  rp->set_state("XX");
  rp->set_prior_margin_mean(race_prior);
  // Chosen so that the model's implied SD(delta_r) equals cfg.race_level_sd.
  rp->set_prior_margin_sd(std::sqrt(cfg.national_sd * cfg.national_sd + cfg.race_level_sd * cfg.race_level_sd + idio));
  rp->set_national_beta(1.0);

  for (std::size_t c = 0; c < expected.size(); ++c) {
    auto* cp = prior.add_counties();
    cp->set_race_id(race_id);
    cp->set_state("XX");
    cp->set_county_fips("99" + std::to_string(100 + c));
    cp->set_expected_total_votes(expected[c]);
    cp->set_expected_total_sd_frac(cfg.turnout_sd_frac);
    cp->set_prior_margin(county_prior[c]);
    cp->set_prior_margin_sd(cfg.county_sd);
    for (const auto& t : types) AddVoteType(cp, t.type, t.share, t.offset, t.order);
  }

  // Truth.
  night.true_national = cfg.national_sd * z(rng);
  const double delta = cfg.race_level_sd * z(rng);
  struct Pending {
    double key;
    std::size_t county;
    std::size_t type;
    int chunk;
  };
  std::vector<Pending> order;
  // chunk_votes[c][t][k] = (side_a, rep, total) for chunk k
  struct Votes {
    uint64_t a, r, total;
  };
  std::vector<std::vector<std::vector<Votes>>> chunks(expected.size());
  double sum_a = 0.0;
  double sum_r = 0.0;
  const int k_chunks = cfg.chunks_per_type;
  const double chunk_sd = cfg.params.within_type_sd * std::sqrt(static_cast<double>(k_chunks));
  for (std::size_t c = 0; c < expected.size(); ++c) {
    const double eta = cfg.county_sd * z(rng);
    const double turnout = expected[c] * (1.0 + cfg.turnout_sd_frac * z(rng));
    chunks[c].resize(std::size(types));
    for (std::size_t t = 0; t < std::size(types); ++t) {
      const double zeta = cfg.params.type_sd * z(rng);
      const double type_margin = county_prior[c] + types[t].offset + night.true_national + delta + eta + zeta;
      std::vector<double> noise(static_cast<std::size_t>(k_chunks));
      double mean_noise = 0.0;
      for (double& e : noise) mean_noise += (e = chunk_sd * z(rng));
      mean_noise /= k_chunks;
      const double type_votes = turnout * types[t].share;
      for (int k = 0; k < k_chunks; ++k) {
        const double m = std::clamp(type_margin + noise[static_cast<std::size_t>(k)] - mean_noise, -99.0, 99.0);
        const double two_party = type_votes * cfg.two_party_share / k_chunks;
        const auto a = static_cast<uint64_t>(std::llround(two_party * (1.0 + m / 100.0) / 2.0));
        const auto r = static_cast<uint64_t>(std::llround(two_party)) - a;
        const auto total = static_cast<uint64_t>(std::llround(type_votes / k_chunks));
        chunks[c][t].push_back({a, r, std::max(total, a + r)});
        sum_a += static_cast<double>(a);
        sum_r += static_cast<double>(r);
        order.push_back({types[t].order + 0.5 * u(rng) + 0.05 * k, c, t, k});
      }
    }
  }
  night.true_margin = 100.0 * (sum_a - sum_r) / (sum_a + sum_r);

  std::sort(order.begin(), order.end(), [](const Pending& x, const Pending& y) { return x.key < y.key; });
  std::vector<std::vector<Votes>> cumulative(expected.size(), std::vector<Votes>(std::size(types), Votes{0, 0, 0}));
  std::vector<std::vector<uint32_t>> chunks_in(expected.size(), std::vector<uint32_t>(std::size(types), 0));
  uint64_t batch = 0;
  for (const Pending& p : order) {
    Votes& cum = cumulative[p.county][p.type];
    const Votes& add = chunks[p.county][p.type][static_cast<std::size_t>(p.chunk)];
    cum.a += add.a;
    cum.r += add.r;
    cum.total += add.total;
    SynthEvent ev;
    susq::v1::CountyResult& r = ev.result;
    r.set_race_id(race_id);
    r.set_state("XX");
    r.set_county_fips("99" + std::to_string(100 + p.county));
    r.set_source("synthetic");
    r.set_vote_type(types[p.type].type);
    auto* d = r.add_votes();
    d->set_candidate("Dem");
    d->set_party("DEM");
    d->set_votes(cum.a);
    auto* rep = r.add_votes();
    rep->set_candidate("Rep");
    rep->set_party("REP");
    rep->set_votes(cum.r);
    r.set_total_votes(cum.total);
    // Reporting units ("precincts") of this county x vote type: one per batch.
    r.set_precincts_reporting(++chunks_in[p.county][p.type]);
    r.set_precincts_total(static_cast<uint32_t>(k_chunks));
    r.set_batch_id("b" + std::to_string(seed) + "-" + std::to_string(batch++));
    night.events.push_back(std::move(ev));
  }
  return night;
}

}  // namespace susq::needle::testing
