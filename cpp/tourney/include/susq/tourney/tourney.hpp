// P(1st), P(top 3) and expected prize for a candidate book against the modeled field.
//
// Official Rules §3: prizes $30,000 / $5,000 / $2,500, no tiebreakers; tied players share the
// pooled prizes of the places they occupy. With A players strictly above you and T tied with
// you, you occupy places A+1 .. A+1+T and receive (sum of those prizes) / (T+1).
#pragma once

#include <cstdint>
#include <vector>

#include "susq/tourney/field.hpp"

namespace susq::tourney {

struct Prizes {
  std::vector<double> amounts{30000.0, 5000.0, 2500.0};  ///< by place, 1st first
};

struct Position {
  uint32_t contract = 0;
  double shares = 0.0;   ///< YES shares; each pays 1 SUSQie if the contract resolves YES
};

/// A final book: cash plus YES shares, each share paying 1 unit if its contract resolves YES.
/// Final multiple in world w = (cash + sum shares * pays) / start_balance.
struct Book {
  double cash = 0.0;
  std::vector<Position> positions;
  double start_balance = 100000.0;
};

struct BookEvaluation {
  double p_first = 0.0;          ///< E[share of 1st place], ties split
  double p_top3 = 0.0;           ///< E[share of places 1-3 occupied], ties split
  double expected_prize = 0.0;   ///< dollars
  std::vector<double> multiple;          ///< per world
  std::vector<double> p_first_by_world;  ///< per world
};

/// Expected payout share when each of n_others players independently lands strictly above
/// (prob `above`) or tied (prob `tie`) with you: E[(sum of prizes of places occupied)/(T+1)].
/// Closed form via Binomial identities; O(len(prizes)).
double ExpectedPrizeShare(uint32_t n_others, double above, double tie, const std::vector<double>& prizes);

/// Evaluates a book against the field in every world. Multithreaded over worlds.
BookEvaluation EvaluateBook(const FieldModel& field, const Book& book, const Prizes& prizes = {},
                            unsigned threads = 0);

/// P(1st) if your multiple were exactly m in every world, as a function of m.
double ProbFirstAtMultiple(const FieldModel& field, double m);

/// P(T <= m) for the field maximum T, averaged over worlds, at each requested m.
std::vector<double> ThresholdCdf(const FieldModel& field, const std::vector<double>& m_values,
                                 unsigned threads = 0);

/// Quantiles of the field maximum T (world-weighted). Exact at atoms; the continuous consensus
/// range is resolved on a 1e-3-wide grid.
std::vector<double> ThresholdQuantiles(const FieldModel& field, const std::vector<double>& probs,
                                       unsigned threads = 0);

}  // namespace susq::tourney
