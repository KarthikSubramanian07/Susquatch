#include "susq/needle/model.hpp"

#include <algorithm>
#include <cmath>
#include <limits>
#include <numeric>
#include <stdexcept>

namespace susq::needle {
namespace {

constexpr int kMixed = susq::v1::VOTE_TYPE_MIXED;

double NormalCdf(double z) { return 0.5 * std::erfc(-z / std::sqrt(2.0)); }

[[noreturn]] void Fail(const std::string& what) { throw std::invalid_argument("needle prior: " + what); }

void RequireFinite(double v, const std::string& what) {
  if (!std::isfinite(v)) Fail(what + " is not finite");
}

// Maps a wire vote type to a slot; -1 for values this build does not know.
int SlotOf(int vote_type) {
  if (vote_type == susq::v1::VOTE_TYPE_UNSPECIFIED) return kMixed;
  if (vote_type < 0 || vote_type > kMixed) return -1;
  return vote_type;
}

std::string CountyKey(const std::string& race, const std::string& fips) { return race + '|' + fips; }

double Margin(double a, double r) { return a + r > 0.0 ? 100.0 * (a - r) / (a + r) : 0.0; }

// Votes still to come in a unit with prior expectation `expected`, `reported` counted so far and
// source-reported progress (precincts reporting / total, NaN if unknown). With progress p the
// unit total is estimated as p * (reported / p) + (1 - p) * expected, i.e. the source's own
// projection trusted in proportion to how much it has counted; remaining = (1 - p) * that.
double RemainingVotes(double expected, double reported, double progress) {
  if (std::isnan(progress) || progress <= 0.0) return std::max(expected - reported, 0.0);
  if (progress >= 1.0) return 0.0;
  return (1.0 - progress) * (reported + (1.0 - progress) * expected);
}

double ReportedFraction(double reported, double remaining) {
  return reported + remaining > 0.0 ? reported / (reported + remaining) : 1.0;
}

// One informative look at a county: reported votes of one vote type (or a mixed tally).
struct Cell {
  int slot = -1;        // -1 = mixed tally spanning several types
  double margin = 0.0;  // reported two-party margin
  double prior = 0.0;   // prior margin of the votes it covers
  double votes = 0.0;   // two-party votes
  double frac = 1.0;    // share of the covered votes already reported
  double var = 0.0;     // type_sd^2 + batch noise: variance of (margin - prior - D - eta)
};

}  // namespace

const char* ToString(IngestStatus status) {
  switch (status) {
    case IngestStatus::kAccepted: return "accepted";
    case IngestStatus::kUnchanged: return "unchanged";
    case IngestStatus::kDuplicateBatch: return "duplicate_batch";
    case IngestStatus::kRegression: return "regression";
    case IngestStatus::kUnknownRace: return "unknown_race";
    case IngestStatus::kUnmodeledCounty: return "unmodeled_county";
    case IngestStatus::kInvalid: return "invalid";
  }
  return "unknown";
}

double NeedleModel::SourceTotals::Progress() const {
  if (precincts_total == 0) return std::numeric_limits<double>::quiet_NaN();
  return std::min(1.0, static_cast<double>(precincts_reporting) / precincts_total);
}

const NeedleModel::SourceTotals* NeedleModel::TypeSlot::Best() const {
  const SourceTotals* best = nullptr;
  for (const auto& s : sources) {
    if (best == nullptr || s.total > best->total) best = &s;
  }
  return best;
}

double NeedleModel::CellNoiseVar(double reported_frac) const {
  const double f = std::clamp(reported_frac, 1e-3, 1.0);
  return params_.within_type_sd * params_.within_type_sd * (1.0 - f) / f;
}

NeedleModel::NeedleModel(const susq::v1::NeedlePrior& prior, NeedleParams params) : params_(params) {
  RequireFinite(prior.national_shift_prior_sd(), "national_shift_prior_sd");
  national_prior_var_ = std::pow(std::max(0.0, prior.national_shift_prior_sd()), 2);

  for (const auto& rp : prior.races()) {
    if (rp.race_id().empty()) Fail("race with empty race_id");
    RequireFinite(rp.prior_margin_mean(), rp.race_id() + " prior_margin_mean");
    RequireFinite(rp.prior_margin_sd(), rp.race_id() + " prior_margin_sd");
    RequireFinite(rp.national_beta(), rp.race_id() + " national_beta");
    if (!race_index_.emplace(rp.race_id(), static_cast<int>(races_.size())).second) {
      Fail("duplicate race " + rp.race_id());
    }
    Race r;
    r.id = rp.race_id();
    r.state = rp.state();
    r.prior_mean = rp.prior_margin_mean();
    r.prior_sd = std::abs(rp.prior_margin_sd());
    r.beta = rp.national_beta();
    r.independent_major = rp.independent_major();
    races_.push_back(std::move(r));
  }

  for (const auto& cp : prior.counties()) {
    auto rit = race_index_.find(cp.race_id());
    if (rit == race_index_.end()) Fail("county " + cp.county_fips() + " of unknown race " + cp.race_id());
    if (cp.county_fips().empty()) Fail("county with empty fips in " + cp.race_id());
    const std::string key = CountyKey(cp.race_id(), cp.county_fips());
    if (!county_index_.emplace(key, static_cast<int>(counties_.size())).second) Fail("duplicate county " + key);
    for (double v : {cp.expected_total_votes(), cp.expected_total_sd_frac(), cp.prior_margin(), cp.prior_margin_sd()}) {
      RequireFinite(v, "county " + key);
    }
    if (cp.expected_total_votes() < 0.0 || cp.expected_total_sd_frac() < 0.0) Fail("negative turnout in " + key);
    County c;
    c.race = rit->second;
    c.fips = cp.county_fips();
    c.expected_total = cp.expected_total_votes();
    c.sd_frac = cp.expected_total_sd_frac();
    c.prior_margin = cp.prior_margin();
    c.prior_sd = std::abs(cp.prior_margin_sd());
    double share_sum = 0.0;
    for (const auto& vt : cp.vote_types()) {
      const int slot = SlotOf(vt.vote_type());
      if (slot < 0) Fail("unknown vote type in " + key);
      RequireFinite(vt.share_of_total(), "vote type share in " + key);
      RequireFinite(vt.margin_offset(), "vote type offset in " + key);
      TypeSlot& s = c.slots[static_cast<std::size_t>(slot)];
      s.in_prior = true;
      s.share += std::max(0.0, vt.share_of_total());
      s.offset = vt.margin_offset();
      s.report_order = vt.typical_report_order();
      share_sum += std::max(0.0, vt.share_of_total());
    }
    if (share_sum <= 0.0) {  // no split known: one mixed type
      for (auto& s : c.slots) s = TypeSlot{};
      c.slots[kMixed].in_prior = true;
      c.slots[kMixed].share = 1.0;
    } else {
      for (auto& s : c.slots) s.share /= share_sum;
    }
    races_[static_cast<std::size_t>(c.race)].counties.push_back(static_cast<int>(counties_.size()));
    counties_.push_back(std::move(c));
  }

  // Align county priors with the race prior (uniform swing) and size delta_r so that the prior
  // SD of the race margin equals the race's prior_margin_sd.
  const double tp = params_.default_two_party_share;
  const double type_var = params_.type_sd * params_.type_sd;
  for (Race& race : races_) {
    double total = 0.0;
    double weighted = 0.0;
    for (int ci : race.counties) {
      const County& c = counties_[static_cast<std::size_t>(ci)];
      double type_mean = 0.0;
      for (const auto& s : c.slots) type_mean += s.share * s.offset;
      total += c.expected_total;
      weighted += c.expected_total * (c.prior_margin + type_mean);
    }
    const double shift = total > 0.0 ? race.prior_mean - weighted / total : 0.0;
    double idiosyncratic = 0.0;
    for (int ci : race.counties) {
      County& c = counties_[static_cast<std::size_t>(ci)];
      c.prior_margin += shift;
      if (total <= 0.0) continue;
      double share_sq = 0.0;
      double type_mean = 0.0;
      for (const auto& s : c.slots) {
        share_sq += s.share * s.share;
        type_mean += s.share * s.offset;
      }
      const double w = c.expected_total / total;
      idiosyncratic += w * w * (c.prior_sd * c.prior_sd + type_var * share_sq);
      const double turnout_sd = c.sd_frac * c.expected_total * tp / (total * tp);
      idiosyncratic += std::pow((c.prior_margin + type_mean - race.prior_mean) * turnout_sd, 2);
    }
    const double floor_var = params_.race_level_sd_floor * params_.race_level_sd_floor;
    race.delta_var = std::max(race.prior_sd * race.prior_sd - race.beta * race.beta * national_prior_var_ -
                                  idiosyncratic,
                              floor_var);
  }
  Recompute();
}

IngestStatus NeedleModel::Ingest(const susq::v1::CountyResult& result, bool is_correction) {
  auto count = [&](IngestStatus s) {
    switch (s) {
      case IngestStatus::kAccepted: ++counters_.accepted; break;
      case IngestStatus::kUnchanged: ++counters_.unchanged; break;
      case IngestStatus::kDuplicateBatch: ++counters_.duplicate_batch; break;
      case IngestStatus::kRegression: ++counters_.regression; break;
      case IngestStatus::kUnknownRace: ++counters_.unknown_race; break;
      case IngestStatus::kUnmodeledCounty: ++counters_.unmodeled_county; break;
      case IngestStatus::kInvalid: ++counters_.invalid; break;
    }
    return s;
  };

  auto rit = race_index_.find(result.race_id());
  if (rit == race_index_.end()) return count(IngestStatus::kUnknownRace);
  Race& race = races_[static_cast<std::size_t>(rit->second)];
  const int slot = SlotOf(result.vote_type());
  if (slot < 0 || result.county_fips().empty() || result.votes_size() == 0) return count(IngestStatus::kInvalid);

  SourceTotals totals;
  totals.source = result.source();
  uint64_t candidate_sum = 0;
  for (const auto& cv : result.votes()) {
    const uint64_t v = cv.votes();
    if (static_cast<double>(v) > params_.max_county_votes) return count(IngestStatus::kInvalid);
    candidate_sum += v;
    if (cv.party() == "DEM") {
      totals.side_a += v;
      totals.dem += v;
    } else if (cv.party() == "REP") {
      totals.rep += v;
    } else if (cv.party() == "IND" && race.independent_major) {
      totals.side_a += v;
      totals.ind += v;
    }
  }
  totals.total = std::max<uint64_t>(result.total_votes(), candidate_sum);
  totals.precincts_reporting = result.precincts_reporting();
  totals.precincts_total = result.precincts_total();
  if (static_cast<double>(totals.total) > params_.max_county_votes) return count(IngestStatus::kInvalid);
  if (result.total_votes() != 0 && result.total_votes() < candidate_sum) return count(IngestStatus::kInvalid);

  if (!result.batch_id().empty() && seen_batches_.contains(result.batch_id())) {
    return count(IngestStatus::kDuplicateBatch);
  }

  const std::string key = CountyKey(result.race_id(), result.county_fips());
  auto cit = county_index_.find(key);
  bool unmodeled = false;
  if (cit == county_index_.end()) {
    County c;
    c.race = rit->second;
    c.fips = result.county_fips();
    c.modeled = false;
    cit = county_index_.emplace(key, static_cast<int>(counties_.size())).first;
    race.counties.push_back(static_cast<int>(counties_.size()));
    counties_.push_back(std::move(c));
    unmodeled = true;
  } else {
    unmodeled = !counties_[static_cast<std::size_t>(cit->second)].modeled;
  }
  TypeSlot& type = counties_[static_cast<std::size_t>(cit->second)].slots[static_cast<std::size_t>(slot)];
  auto sit = std::find_if(type.sources.begin(), type.sources.end(),
                          [&](const SourceTotals& s) { return s.source == totals.source; });
  if (sit != type.sources.end()) {
    const bool same = sit->total == totals.total && sit->side_a == totals.side_a && sit->rep == totals.rep;
    const bool regressed = totals.total < sit->total || totals.side_a < sit->side_a || totals.rep < sit->rep;
    if (same) {
      if (!result.batch_id().empty()) seen_batches_.insert(result.batch_id());
      return count(IngestStatus::kUnchanged);
    }
    if (regressed && !(is_correction || result.is_correction())) {
      return count(IngestStatus::kRegression);
    }
    *sit = totals;
  } else {
    type.sources.push_back(totals);
  }
  if (!result.batch_id().empty()) seen_batches_.insert(result.batch_id());
  ++race.n_batches;
  return count(unmodeled ? IngestStatus::kUnmodeledCounty : IngestStatus::kAccepted);
}

std::size_t NeedleModel::IngestBatch(const susq::v1::CountyResultsBatch& batch) {
  std::size_t accepted = 0;
  for (const auto& r : batch.results()) {
    const IngestStatus s = Ingest(r);
    accepted += (s == IngestStatus::kAccepted || s == IngestStatus::kUnmodeledCounty) ? 1 : 0;
  }
  Recompute();
  return accepted;
}

void NeedleModel::Recompute() {
  const double type_var = params_.type_sd * params_.type_sd;
  const double nan = std::numeric_limits<double>::quiet_NaN();

  // Pass 1: per-county reported totals, remaining votes by type, and informative cells.
  struct CountyCalc {
    double reported = 0.0, rep_a = 0.0, rep_r = 0.0, rep_dem = 0.0, rep_ind = 0.0;
    std::array<double, kSlots> rem{};
    std::array<double, kSlots> prior{};
    std::vector<Cell> cells;
    double sum_inv_var = 0.0;
    double residual = 0.0;
    double residual_var = 0.0;
  };
  std::vector<CountyCalc> calc(counties_.size());
  for (std::size_t ci = 0; ci < counties_.size(); ++ci) {
    const County& c = counties_[ci];
    CountyCalc& k = calc[ci];
    for (int s = 0; s < kSlots; ++s) {
      k.prior[static_cast<std::size_t>(s)] = c.prior_margin + c.slots[static_cast<std::size_t>(s)].offset;
    }
    auto add = [&](const SourceTotals& t) {
      k.reported += static_cast<double>(t.total);
      k.rep_a += static_cast<double>(t.side_a);
      k.rep_r += static_cast<double>(t.rep);
      k.rep_dem += static_cast<double>(t.dem);
      k.rep_ind += static_cast<double>(t.ind);
    };
    double typed_total = 0.0;
    for (int s = 0; s < kMixed; ++s) {
      if (const SourceTotals* b = c.slots[static_cast<std::size_t>(s)].Best()) typed_total += static_cast<double>(b->total);
    }
    const SourceTotals* mixed = c.slots[kMixed].Best();
    const double e = c.expected_total;

    if (mixed != nullptr && static_cast<double>(mixed->total) > typed_total) {
      // Source does not split by type: fill types in their typical reporting order.
      add(*mixed);
      std::vector<int> order;
      for (int s = 0; s < kSlots; ++s) {
        if (c.slots[static_cast<std::size_t>(s)].in_prior) order.push_back(s);
      }
      std::stable_sort(order.begin(), order.end(), [&](int a, int b) {
        return c.slots[static_cast<std::size_t>(a)].report_order < c.slots[static_cast<std::size_t>(b)].report_order;
      });
      double left = k.reported;
      double alloc_sum = 0.0;
      double alloc_prior = 0.0;
      double leftover_sum = 0.0;
      for (int s : order) {
        const auto us = static_cast<std::size_t>(s);
        const double expected = e * c.slots[us].share;
        const double alloc = std::min(left, expected);
        left -= alloc;
        k.rem[us] = std::max(expected - alloc, 0.0);
        leftover_sum += k.rem[us];
        alloc_sum += alloc;
        alloc_prior += alloc * k.prior[us];
      }
      // Rescale the per-type leftovers to the county's remaining-vote estimate.
      const double county_rem = RemainingVotes(e, k.reported, mixed->Progress());
      for (int s : order) {
        const auto us = static_cast<std::size_t>(s);
        k.rem[us] = leftover_sum > 0.0 ? k.rem[us] * county_rem / leftover_sum : 0.0;
      }
      if (leftover_sum <= 0.0 && !order.empty()) k.rem[static_cast<std::size_t>(order.back())] = county_rem;
      if (c.modeled && k.rep_a + k.rep_r > 0.0) {
        k.cells.push_back({-1, Margin(k.rep_a, k.rep_r), alloc_sum > 0.0 ? alloc_prior / alloc_sum : c.prior_margin,
                           k.rep_a + k.rep_r, ReportedFraction(k.reported, county_rem)});
      }
    } else {
      for (int s = 0; s < kSlots; ++s) {
        const auto us = static_cast<std::size_t>(s);
        const TypeSlot& slot = c.slots[us];
        const SourceTotals* b = s == kMixed ? nullptr : slot.Best();
        const double expected = e * slot.share;
        const double got = b != nullptr ? static_cast<double>(b->total) : 0.0;
        const double progress = b != nullptr ? b->Progress() : std::numeric_limits<double>::quiet_NaN();
        if (slot.in_prior) k.rem[us] = RemainingVotes(expected, got, progress);
        if (b == nullptr) continue;
        add(*b);
        const double two_party = static_cast<double>(b->side_a + b->rep);
        if (c.modeled && two_party > 0.0) {
          k.cells.push_back({s, Margin(static_cast<double>(b->side_a), static_cast<double>(b->rep)), k.prior[us],
                             two_party, ReportedFraction(got, k.rem[us])});
        }
      }
    }
    // County summary for the race-level update: cells share eta_c, so with per-cell noise
    // t_v = type_sd^2 + batch noise, the information about the race deviation is the
    // 1/t-weighted mean residual with variance sd_eta^2 + 1/sum(1/t) (Sherman-Morrison).
    for (Cell& cell : k.cells) {
      cell.var = std::max(type_var + CellNoiseVar(cell.frac), params_.obs_var_floor);
      k.sum_inv_var += 1.0 / cell.var;
      k.residual += (cell.margin - cell.prior) / cell.var;
    }
    if (!k.cells.empty()) {
      k.residual /= k.sum_inv_var;
      k.residual_var = std::max(c.prior_sd * c.prior_sd + 1.0 / k.sum_inv_var, params_.obs_var_floor);
    }
  }

  // Pass 2: race summaries z_r with precision P_r, then the national shift.
  std::vector<double> precision(races_.size(), 0.0);
  std::vector<double> z(races_.size(), 0.0);
  for (std::size_t ri = 0; ri < races_.size(); ++ri) {
    double sum = 0.0;
    for (int ci : races_[ri].counties) {
      const CountyCalc& k = calc[static_cast<std::size_t>(ci)];
      if (k.cells.empty()) continue;
      precision[ri] += 1.0 / k.residual_var;
      sum += k.residual / k.residual_var;
    }
    if (precision[ri] > 0.0) z[ri] = sum / precision[ri];
  }
  double n_mean = 0.0;
  double n_var = national_prior_var_;
  national_.n_races_informing = 0;
  if (national_prior_var_ > 0.0) {
    double tau = 1.0 / national_prior_var_;
    double num = 0.0;
    for (std::size_t ri = 0; ri < races_.size(); ++ri) {
      if (precision[ri] <= 0.0) continue;
      const double v = races_[ri].delta_var + 1.0 / precision[ri];
      tau += races_[ri].beta * races_[ri].beta / v;
      num += races_[ri].beta * z[ri] / v;
      ++national_.n_races_informing;
    }
    n_var = 1.0 / tau;
    n_mean = num * n_var;
  }
  national_.mean = n_mean;
  national_.sd = std::sqrt(n_var);

  // Pass 3: race deviation D_r = beta N + delta_r, county shrinkage, remaining-vote aggregation.
  estimates_.assign(races_.size(), RaceEstimate{});
  for (std::size_t ri = 0; ri < races_.size(); ++ri) {
    const Race& race = races_[ri];
    const double gain = precision[ri] > 0.0 ? race.delta_var / (race.delta_var + 1.0 / precision[ri]) : 0.0;
    const double dev = race.beta * n_mean + gain * (z[ri] - race.beta * n_mean);
    const double dev_var = std::pow(race.beta * (1.0 - gain), 2) * n_var + race.delta_var * (1.0 - gain);

    double rep_a = 0.0, rep_r = 0.0, rep_total = 0.0, rep_dem = 0.0, rep_ind = 0.0;
    double rem_total = 0.0, rem2p_total = 0.0, rem2p_margin = 0.0;
    uint32_t reporting = 0;
    // For each county, the remaining two-party votes' total margin L_c = sum_v b_v (eta + zeta_v)
    // has posterior mean linear in D (coefficient kappa_c) and conditional variance tau_c^2, so
    // Var(sum over counties) = (sum kappa)^2 Var(D) + sum tau^2 exactly (docs/MODELS.md §3.3).
    struct Remaining {
      double margin, turnout_sd;
    };
    std::vector<Remaining> rems;
    double kappa_sum = 0.0;
    double tau2_sum = 0.0;
    for (int ci : race.counties) {
      County& c = counties_[static_cast<std::size_t>(ci)];
      const CountyCalc& k = calc[static_cast<std::size_t>(ci)];
      rep_a += k.rep_a;
      rep_r += k.rep_r;
      rep_total += k.reported;
      rep_dem += k.rep_dem;
      rep_ind += k.rep_ind;
      reporting += k.reported > 0.0 ? 1 : 0;

      const double eta_prec = (c.prior_sd > 0.0 ? 1.0 / (c.prior_sd * c.prior_sd) : 1e12) + k.sum_inv_var;
      double eta_num = 0.0;
      for (const Cell& cell : k.cells) eta_num += (cell.margin - cell.prior - dev) / cell.var;
      const double eta = eta_num / eta_prec;
      const double eta_gain = k.sum_inv_var / eta_prec;  // d eta / d(-D)
      std::array<double, kSlots> zeta{};
      std::array<double, kSlots> zeta_gain{};   // h_v; 0 = type not reported
      std::array<double, kSlots> reported2p{};  // typed cells only
      std::array<double, kSlots> reported_margin{};
      for (const Cell& cell : k.cells) {
        if (cell.slot < 0) continue;
        const auto us = static_cast<std::size_t>(cell.slot);
        zeta_gain[us] = type_var / cell.var;
        zeta[us] = zeta_gain[us] * (cell.margin - cell.prior - dev - eta);
        reported2p[us] = cell.votes;
        reported_margin[us] = cell.margin;
      }

      const double tp = k.reported > 0.0 && k.rep_a + k.rep_r > 0.0 ? (k.rep_a + k.rep_r) / k.reported
                                                                     : params_.default_two_party_share;
      // The latent m_v is the margin of ALL type-v votes; the reported part is known, so the
      // uncounted part contributes b_v * m_v - reported_v * y_v with b_v = reported + remaining.
      double c_rem = 0.0, c_rem2p = 0.0, c_margin_sum = 0.0;
      double b_sum = 0.0;    // sum_v b_v
      double b_h = 0.0;      // sum_v b_v h_v
      double zeta_var = 0.0; // sum_v b_v^2 Var(zeta_v | eta, data)
      for (int s = 0; s < kSlots; ++s) {
        const auto us = static_cast<std::size_t>(s);
        if (k.rem[us] <= 0.0) continue;
        const double rem2p = k.rem[us] * tp;
        const double b = rem2p + reported2p[us];
        c_rem += k.rem[us];
        c_rem2p += rem2p;
        c_margin_sum += b * (k.prior[us] + dev + eta + zeta[us]) - reported2p[us] * reported_margin[us];
        b_sum += b;
        b_h += b * zeta_gain[us];
        zeta_var += b * b * type_var * (1.0 - zeta_gain[us]);
      }
      c.eta = eta;
      c.reported = k.reported;
      c.reported_margin = k.rep_a + k.rep_r > 0.0 ? Margin(k.rep_a, k.rep_r) : nan;
      c.remaining = c_rem;
      c.remaining_margin = c_rem2p > 0.0 ? c_margin_sum / c_rem2p : nan;
      if (c_rem2p <= 0.0) continue;
      const double c_eta = b_sum - b_h;
      kappa_sum += c_eta * (1.0 - eta_gain);
      tau2_sum += c_eta * c_eta / eta_prec + zeta_var;
      const double turnout_sd =
          c.expected_total > 0.0 ? c.sd_frac * c.expected_total * std::sqrt(c_rem / c.expected_total) * tp : 0.0;
      rems.push_back({c_margin_sum / c_rem2p, turnout_sd});
      rem_total += c_rem;
      rem2p_total += c_rem2p;
      rem2p_margin += c_margin_sum;
    }

    RaceEstimate& est = estimates_[ri];
    est.race_id = race.id;
    const double w_total = rep_a + rep_r + rem2p_total;
    double var = 0.0;
    if (w_total > 0.0) {
      est.margin_mean = (100.0 * (rep_a - rep_r) + rem2p_margin) / w_total;
      var = (kappa_sum * kappa_sum * dev_var + tau2_sum) / (w_total * w_total);
      for (const Remaining& r : rems) var += std::pow((r.margin - est.margin_mean) * r.turnout_sd / w_total, 2);
    } else {
      est.margin_mean = race.prior_mean + dev;
      var = dev_var;
    }
    est.margin_sd = std::max(std::sqrt(var), params_.margin_sd_floor);
    const double p_side_a = NormalCdf(est.margin_mean / est.margin_sd);
    est.p_rep = 1.0 - p_side_a;
    if (race.independent_major) {
      const double dem_share = rep_dem + rep_ind > 0.0 ? rep_dem / (rep_dem + rep_ind) : 0.0;
      est.p_dem = p_side_a * dem_share;
    } else {
      est.p_dem = p_side_a;
    }
    est.expected_remaining_votes = rem_total;
    est.reported_frac = rep_total + rem_total > 0.0 ? rep_total / (rep_total + rem_total) : 0.0;
    est.n_batches = race.n_batches;
    est.n_counties_reporting = reporting;
  }
}

double NeedleModel::RaceLevelSd(const std::string& race_id) const {
  auto it = race_index_.find(race_id);
  if (it == race_index_.end()) return std::numeric_limits<double>::quiet_NaN();
  return std::sqrt(races_[static_cast<std::size_t>(it->second)].delta_var);
}

std::vector<CountyEstimate> NeedleModel::Counties() const {
  std::vector<CountyEstimate> out;
  out.reserve(counties_.size());
  for (const County& c : counties_) {
    CountyEstimate e;
    e.race_id = races_[static_cast<std::size_t>(c.race)].id;
    e.county_fips = c.fips;
    e.modeled = c.modeled;
    e.reported_votes = c.reported;
    e.reported_margin = c.reported_margin;
    e.expected_remaining_votes = c.remaining;
    e.remaining_margin = c.remaining_margin;
    e.county_deviation = c.eta;
    out.push_back(std::move(e));
  }
  return out;
}

void NeedleModel::FillRacePosteriors(susq::v1::RacePosteriors* out) const {
  out->clear_races();
  for (const RaceEstimate& e : estimates_) {
    susq::v1::RacePosterior* p = out->add_races();
    p->set_race_id(e.race_id);
    p->set_p_dem(e.p_dem);
    p->set_p_rep(e.p_rep);
    p->set_margin_mean(e.margin_mean);
    p->set_margin_sd(e.margin_sd);
    p->set_expected_remaining_votes(e.expected_remaining_votes);
    p->set_reported_frac(e.reported_frac);
    p->set_n_batches(e.n_batches);
    p->set_n_counties_reporting(e.n_counties_reporting);
  }
}

void NeedleModel::FillNationalShift(susq::v1::NationalShift* out) const {
  out->set_mean(national_.mean);
  out->set_sd(national_.sd);
  out->set_n_races_informing(national_.n_races_informing);
}

}  // namespace susq::needle
