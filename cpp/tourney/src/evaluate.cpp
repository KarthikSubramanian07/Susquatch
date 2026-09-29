#include <algorithm>
#include <cmath>
#include <numeric>
#include <stdexcept>

#include "susq/sim/parallel.hpp"
#include "susq/tourney/tourney.hpp"

namespace susq::tourney {
namespace {

using susq::sim::ParallelFor;

// Binomial(n, p) pmf at small k, without lgamma (not thread-safe on every libc).
double BinomPmf(uint32_t n, uint32_t k, double p) {
  if (k > n) return 0.0;
  if (p <= 0.0) return k == 0 ? 1.0 : 0.0;
  if (p >= 1.0) return k == n ? 1.0 : 0.0;
  double log_choose = 0.0;
  for (uint32_t i = 1; i <= k; ++i) log_choose += std::log(static_cast<double>(n - k + i) / i);
  return std::exp(log_choose + k * std::log(p) + (n - k) * std::log1p(-p));
}

// E[1 / (T + 1)] for T ~ Binomial(n, p) = (1 - (1-p)^(n+1)) / ((n+1) p).
double InverseCountExpectation(uint32_t n, double p) {
  if (p <= 0.0) return 1.0;
  const double n1 = static_cast<double>(n) + 1.0;
  if (p >= 1.0) return 1.0 / n1;
  return -std::expm1(n1 * std::log1p(-p)) / (n1 * p);
}

// Deterministic world-chunking so parallel sums do not depend on thread count.
constexpr std::size_t kWorldChunk = 256;

}  // namespace

double ExpectedPrizeShare(uint32_t n_others, double above, double tie, const std::vector<double>& prizes) {
  const std::size_t n_places = prizes.size();
  if (n_places == 0) return 0.0;
  above = std::clamp(above, 0.0, 1.0);
  tie = std::clamp(tie, 0.0, 1.0 - above);
  const double p_tie = above < 1.0 ? std::clamp(tie / (1.0 - above), 0.0, 1.0) : 0.0;

  double total = 0.0;
  const auto max_k = static_cast<uint32_t>(std::min<std::size_t>(n_places - 1, n_others));
  for (uint32_t k = 0; k <= max_k; ++k) {
    const double p_above_k = BinomPmf(n_others, k, above);
    if (p_above_k == 0.0) continue;
    const uint32_t rest = n_others - k;
    // With T >= t_full tied players, every remaining prize place k+1..n_places is occupied.
    const std::size_t t_full = n_places - k - 1;
    const double all_remaining =
        std::accumulate(prizes.begin() + k, prizes.end(), 0.0);
    double partial = 0.0;
    double partial_inverse = 0.0;
    double prizes_so_far = 0.0;
    for (std::size_t t = 0; t < t_full && t <= rest; ++t) {
      prizes_so_far += prizes[k + t];
      const double pt = BinomPmf(rest, static_cast<uint32_t>(t), p_tie);
      partial += pt * prizes_so_far / static_cast<double>(t + 1);
      partial_inverse += pt / static_cast<double>(t + 1);
    }
    const double tail_inverse = std::max(0.0, InverseCountExpectation(rest, p_tie) - partial_inverse);
    total += p_above_k * (partial + all_remaining * tail_inverse);
  }
  return total;
}

BookEvaluation EvaluateBook(const FieldModel& field, const Book& book, const Prizes& prizes,
                            unsigned threads) {
  const Market& market = field.market();
  const std::size_t n_worlds = market.n_worlds;
  if (!(book.start_balance > 0.0)) throw std::invalid_argument("tourney: start_balance must be > 0");
  for (const Position& pos : book.positions) {
    if (pos.contract >= market.prices.size()) throw std::invalid_argument("tourney: position contract out of range");
    if (!std::isfinite(pos.shares)) throw std::invalid_argument("tourney: non-finite position");
  }
  const std::vector<double> first{1.0};
  const std::vector<double> top3{1.0, 1.0, 1.0};
  const uint32_t n = field.params().n_players;

  BookEvaluation out;
  out.multiple.resize(n_worlds);
  out.p_first_by_world.resize(n_worlds);
  std::vector<double> top3_by_world(n_worlds);
  std::vector<double> prize_by_world(n_worlds);
  ParallelFor(n_worlds, threads, [&](std::size_t w) {
    double value = book.cash;
    for (const Position& pos : book.positions) {
      if (market.pays[pos.contract * n_worlds + w]) value += pos.shares;
    }
    const double m = value / book.start_balance;
    const Placement place = field.Compare(w, m);
    out.multiple[w] = m;
    out.p_first_by_world[w] = ExpectedPrizeShare(n, place.above, place.tie, first);
    top3_by_world[w] = ExpectedPrizeShare(n, place.above, place.tie, top3);
    prize_by_world[w] = ExpectedPrizeShare(n, place.above, place.tie, prizes.amounts);
  });
  for (std::size_t w = 0; w < n_worlds; ++w) {
    const double weight = field.world_weight(w);
    out.p_first += weight * out.p_first_by_world[w];
    out.p_top3 += weight * top3_by_world[w];
    out.expected_prize += weight * prize_by_world[w];
  }
  return out;
}

double ProbFirstAtMultiple(const FieldModel& field, double m) {
  double total = 0.0;
  for (std::size_t w = 0; w < field.market().n_worlds; ++w) {
    const Placement place = field.Compare(w, m);
    total += field.world_weight(w) * ExpectedPrizeShare(field.params().n_players, place.above, place.tie, {1.0});
  }
  return total;
}

std::vector<double> ThresholdCdf(const FieldModel& field, const std::vector<double>& m_values,
                                 unsigned threads) {
  const Market& market = field.market();
  const FieldParams& params = field.params();
  const std::size_t n_worlds = market.n_worlds;
  const std::size_t n_m = m_values.size();
  std::vector<std::size_t> order(n_m);
  std::iota(order.begin(), order.end(), 0);
  std::sort(order.begin(), order.end(), [&](std::size_t a, std::size_t b) { return m_values[a] < m_values[b]; });

  // World-independent part of P(player > m): passive, consensus and bots.
  std::vector<double> fixed_above(n_m);
  for (std::size_t i = 0; i < n_m; ++i) fixed_above[i] = field.CompareFixedTypes(m_values[i]).above;

  const std::vector<double>& yolo = field.yolo_weights();
  const std::size_t n_chunks = (n_worlds + kWorldChunk - 1) / kWorldChunk;
  std::vector<double> chunk_sums(n_chunks * n_m, 0.0);
  ParallelFor(n_chunks, threads, [&](std::size_t chunk) {
    std::vector<std::pair<double, double>> values;  // (multiple, choice probability)
    const std::size_t end = std::min(n_worlds, (chunk + 1) * kWorldChunk);
    for (std::size_t w = chunk * kWorldChunk; w < end; ++w) {
      values.clear();
      for (std::size_t j = 0; j < yolo.size(); ++j) {
        if (yolo[j] > 0.0) values.emplace_back(market.pays[j * n_worlds + w] ? 1.0 / market.prices[j] : 0.0, yolo[j]);
      }
      std::sort(values.begin(), values.end());
      double yolo_above = std::accumulate(values.begin(), values.end(), 0.0,
                                          [](double s, const auto& v) { return s + v.second; });
      std::size_t next = 0;  // first value not yet removed from "above"
      for (std::size_t idx : order) {
        const double m = m_values[idx];
        while (next < values.size() && (values[next].first < m || SameMultiple(values[next].first, m))) {
          yolo_above -= values[next].second;
          ++next;
        }
        const double above = std::clamp(fixed_above[idx] + params.f_yolo * std::max(0.0, yolo_above), 0.0, 1.0);
        const double cdf = above >= 1.0 ? 0.0 : std::exp(params.n_players * std::log1p(-above));
        chunk_sums[chunk * n_m + idx] += field.world_weight(w) * cdf;
      }
    }
  });
  std::vector<double> cdf(n_m, 0.0);
  for (std::size_t c = 0; c < n_chunks; ++c) {
    for (std::size_t i = 0; i < n_m; ++i) cdf[i] += chunk_sums[c * n_m + i];
  }
  return cdf;
}

std::vector<double> ThresholdQuantiles(const FieldModel& field, const std::vector<double>& probs,
                                       unsigned threads) {
  const FieldParams& p = field.params();
  std::vector<double> support{0.0, 1.0};
  for (std::size_t j = 0; j < field.yolo_weights().size(); ++j) {
    if (field.yolo_weights()[j] > 0.0) support.push_back(1.0 / field.market().prices[j]);
  }
  for (const auto& atom : field.bot_pmf()) support.push_back(atom.first);
  if (p.f_kelly > 0.0) {
    for (double m = p.kelly_lo; m < p.kelly_hi; m += 1e-3) support.push_back(m);
    support.push_back(p.kelly_hi);
  }
  std::sort(support.begin(), support.end());
  support.erase(std::unique(support.begin(), support.end(), SameMultiple), support.end());
  const std::vector<double> cdf = ThresholdCdf(field, support, threads);

  std::vector<double> out;
  for (double q : probs) {
    if (!(q >= 0.0 && q <= 1.0)) throw std::invalid_argument("tourney: quantile probability not in [0,1]");
    const auto it = std::find_if(cdf.begin(), cdf.end(), [&](double c) { return c >= q - 1e-12; });
    out.push_back(it == cdf.end() ? support.back() : support[static_cast<std::size_t>(it - cdf.begin())]);
  }
  return out;
}

}  // namespace susq::tourney
