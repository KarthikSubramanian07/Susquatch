// Election-night county expected-vote model (SPEC §3 Phase 3, RESEARCH §7, docs/MODELS.md §3).
//
// Margins are D-side minus R in points of the two-party vote. For the remaining (not yet
// counted) votes of vote type v in county c of race r:
//
//   m_cv = p_cv + beta_r * N + delta_r + eta_c + zeta_cv
//
//   p_cv     prior: county prior margin (aligned so counties aggregate to the race prior)
//            plus the vote-type margin offset (mail vs Election Day skew)
//   N        national shift, prior N(0, national_shift_prior_sd^2), shared by every race
//   delta_r  race/state-level deviation, variance chosen so the race prior SD is honoured
//   eta_c    county deviation, SD = county prior_margin_sd
//   zeta_cv  county x vote-type deviation, SD = params.type_sd
//
// Reported votes of type v are a noisy look at the same quantities (early and late batches of
// one type differ: variance within_type_sd^2 * (1-f)/f at reported fraction f). Updates are
// exact Gaussian conditioning given per-county summaries, O(counties) per recompute:
// counties -> race summaries -> national shift N -> race deviations -> county/type shrinkage.
#pragma once

#include <array>
#include <cstdint>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <vector>

#include "susq/contracts.hpp"

namespace susq::needle {

struct NeedleParams {
  double within_type_sd = 6.0;          ///< early-vs-late batch gap within a type, at f = 1/2 (pts)
  double type_sd = 3.0;                 ///< county x vote-type deviation SD (pts)
  double race_level_sd_floor = 1.0;     ///< lower bound on SD(delta_r)
  double default_two_party_share = 0.97;///< two-party share of a county's votes before it reports
  double margin_sd_floor = 0.05;        ///< lower bound on a reported margin SD (pts)
  double obs_var_floor = 0.25;          ///< lower bound on any observation variance (pts^2)
  double max_county_votes = 5e7;        ///< results above this are malformed
};

enum class IngestStatus {
  kAccepted,          ///< new cumulative totals stored
  kUnchanged,         ///< same totals as already stored for this source
  kDuplicateBatch,    ///< batch_id already seen
  kRegression,        ///< cumulative totals went down and the result is not a correction
  kUnknownRace,       ///< race not in the prior
  kUnmodeledCounty,   ///< accepted, but the county is not in the prior (counted, not modeled)
  kInvalid,           ///< malformed content
};

const char* ToString(IngestStatus status);

struct IngestCounters {
  uint64_t accepted = 0;
  uint64_t unchanged = 0;
  uint64_t duplicate_batch = 0;
  uint64_t regression = 0;
  uint64_t unknown_race = 0;
  uint64_t unmodeled_county = 0;
  uint64_t invalid = 0;
};

struct RaceEstimate {
  std::string race_id;
  double p_dem = 0.0;
  double p_rep = 0.0;
  double margin_mean = 0.0;
  double margin_sd = 0.0;
  double expected_remaining_votes = 0.0;
  double reported_frac = 0.0;
  uint32_t n_batches = 0;
  uint32_t n_counties_reporting = 0;
};

struct NationalEstimate {
  double mean = 0.0;
  double sd = 0.0;
  uint32_t n_races_informing = 0;
};

struct CountyEstimate {
  std::string race_id;
  std::string county_fips;
  bool modeled = true;
  double reported_votes = 0.0;
  double reported_margin = 0.0;           ///< NaN before any two-party votes
  double expected_remaining_votes = 0.0;
  double remaining_margin = 0.0;          ///< posterior mean margin of the uncounted votes
  double county_deviation = 0.0;          ///< posterior mean of eta_c
};

class NeedleModel {
 public:
  /// Throws std::invalid_argument when the prior is inconsistent (duplicate ids, counties of
  /// unknown races, non-finite numbers).
  explicit NeedleModel(const susq::v1::NeedlePrior& prior, NeedleParams params = {});

  /// Stores one cumulative county result. Corrections (lower totals) are accepted only with
  /// `is_correction`; the susq.v1 contract has no correction flag yet, so the bus path passes
  /// false. Call Recompute() afterwards (IngestBatch does both).
  IngestStatus Ingest(const susq::v1::CountyResult& result, bool is_correction = false);

  /// Ingests every result of a batch, then recomputes. Returns the number accepted.
  std::size_t IngestBatch(const susq::v1::CountyResultsBatch& batch);

  /// Recomputes every posterior from the stored cumulative totals. O(counties).
  void Recompute();

  const std::vector<RaceEstimate>& races() const { return estimates_; }
  const NationalEstimate& national() const { return national_; }
  std::vector<CountyEstimate> Counties() const;
  const IngestCounters& counters() const { return counters_; }

  /// SD of the race-level deviation delta_r implied by the prior (diagnostics); NaN if unknown.
  double RaceLevelSd(const std::string& race_id) const;

  void FillRacePosteriors(susq::v1::RacePosteriors* out) const;
  void FillNationalShift(susq::v1::NationalShift* out) const;

 private:
  static constexpr int kSlots = 6;  // indexed by susq::v1::VoteType; UNSPECIFIED folds into MIXED

  struct SourceTotals {
    std::string source;
    uint64_t side_a = 0;    // DEM (+ IND in independent-major races)
    uint64_t rep = 0;
    uint64_t dem = 0;
    uint64_t ind = 0;
    uint64_t total = 0;
    uint32_t precincts_reporting = 0;
    uint32_t precincts_total = 0;
    /// Share of the unit already counted according to the source (NaN when unknown).
    double Progress() const;
  };
  struct TypeSlot {
    bool in_prior = false;
    double share = 0.0;
    double offset = 0.0;
    double report_order = 0.0;
    std::vector<SourceTotals> sources;
    const SourceTotals* Best() const;
  };
  struct County {
    int race = 0;
    std::string fips;
    bool modeled = true;
    double expected_total = 0.0;
    double sd_frac = 0.0;
    double prior_margin = 0.0;  // aligned
    double prior_sd = 0.0;
    std::array<TypeSlot, kSlots> slots;
    // Posterior state from the last Recompute (for Counties()).
    double eta = 0.0;
    double remaining = 0.0;
    double remaining_margin = 0.0;
    double reported = 0.0;
    double reported_margin = 0.0;
  };
  struct Race {
    std::string id;
    std::string state;
    double prior_mean = 0.0;
    double prior_sd = 0.0;
    double beta = 1.0;
    bool independent_major = false;
    double delta_var = 0.0;
    std::vector<int> counties;
    uint32_t n_batches = 0;
  };

  double CellNoiseVar(double reported_frac) const;

  NeedleParams params_;
  double national_prior_var_ = 0.0;
  std::vector<Race> races_;
  std::vector<County> counties_;
  std::unordered_map<std::string, int> race_index_;
  std::unordered_map<std::string, int> county_index_;  // key race_id + '|' + fips
  std::unordered_set<std::string> seen_batches_;
  IngestCounters counters_;
  std::vector<RaceEstimate> estimates_;
  NationalEstimate national_;
};

}  // namespace susq::needle
