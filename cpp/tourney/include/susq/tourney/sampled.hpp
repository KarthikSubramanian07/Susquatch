// Sampled field maximum with the research pipeline's semantics (py/susq/models/tourney.py,
// `simulate_threshold`), so `susq.models.facade` can swap in the C++ engine unchanged.
//
// Differences from the exact model in tourney.hpp, kept on purpose for parity with the numpy
// reference: type counts are fixed (round(N * frac), half-to-even), bots are lognormal, every
// contract is YOLO-eligible (prices clipped to [1e-3, 1]) and YOLO picks are independent
// Bernoulli(1 - (1 - w_j)^n_yolo) per contract. Everyone not otherwise typed is passive (1.0x).
#pragma once

#include <cstddef>
#include <cstdint>
#include <vector>

namespace susq::tourney {

struct SampledFieldParams {
  uint32_t n_players = 2000;
  double frac_kelly = 0.35;
  double frac_yolo = 0.15;
  double frac_bot = 0.05;
  double kelly_lo = 0.8;
  double kelly_hi = 1.3;
  double yolo_price_power = 1.0;
  double bot_log_mean = 0.6931471805599453;  // log 2
  double bot_log_sd = 0.8;
};

/// One draw of the field maximum T per world. `outcomes` is (n_worlds, n_contracts) row-major,
/// nonzero = the contract resolves YES. Deterministic in `seed` for any thread count.
std::vector<double> SampleThresholds(const uint8_t* outcomes, std::size_t n_worlds, const std::vector<double>& prices,
                                     const SampledFieldParams& params, uint64_t seed, unsigned threads = 0);

/// Inverse standard-normal CDF (Acklam's rational approximation plus one Halley step; ~1e-15).
double NormalQuantile(double p);

}  // namespace susq::tourney
