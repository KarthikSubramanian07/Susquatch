// Counter-style random streams for reproducible, thread-count-independent Monte Carlo.
//
// Every random quantity in the simulator is drawn from a stream identified by
// (seed, entity key, block index). A block is a fixed-size range of draws, so the
// value of draw d for entity e never depends on which thread produced it, on how
// many threads ran, or on which other entities exist in the configuration.
#pragma once

#include <cmath>
#include <cstdint>
#include <string_view>

namespace susq::sim {

/// One step of SplitMix64 (Steele, Lea & Flood 2014); advances `state`.
inline uint64_t SplitMix64(uint64_t& state) {
  uint64_t z = (state += 0x9E3779B97F4A7C15ULL);
  z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
  z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
  return z ^ (z >> 31);
}

/// 64-bit FNV-1a hash, used to turn entity names ("race:2026-SEN-OH-S") into stream keys.
constexpr uint64_t HashKey(std::string_view s) {
  uint64_t h = 0xCBF29CE484222325ULL;
  for (char c : s) {
    h ^= static_cast<uint8_t>(c);
    h *= 0x100000001B3ULL;
  }
  return h;
}

/// xoshiro256++ (Blackman & Vigna 2019): fast, 256-bit state, passes BigCrush.
class Xoshiro256pp {
 public:
  /// Seeds a stream from (seed, key, block) through SplitMix64 so nearby inputs decorrelate.
  Xoshiro256pp(uint64_t seed, uint64_t key, uint64_t block) {
    uint64_t sm = seed;
    uint64_t mixed = SplitMix64(sm) ^ key;
    mixed = SplitMix64(mixed) ^ block;
    for (auto& word : s_) word = SplitMix64(mixed);
  }

  uint64_t Next() {
    const uint64_t result = Rotl(s_[0] + s_[3], 23) + s_[0];
    const uint64_t t = s_[1] << 17;
    s_[2] ^= s_[0];
    s_[3] ^= s_[1];
    s_[1] ^= s_[2];
    s_[0] ^= s_[3];
    s_[2] ^= t;
    s_[3] = Rotl(s_[3], 45);
    return result;
  }

  /// Uniform on the open interval (0, 1); safe to pass to log().
  double UniformOpen() { return (static_cast<double>(Next() >> 11) + 0.5) * 0x1.0p-53; }

 private:
  static uint64_t Rotl(uint64_t x, int k) { return (x << k) | (x >> (64 - k)); }
  uint64_t s_[4]{};
};

/// Tables for the 128-layer normal ziggurat of Marsaglia & Tsang (2000).
struct ZigguratTables {
  static constexpr int kLayers = 128;
  static constexpr double kR = 3.442619855899;  // start of the tail
  double k[kLayers]{};  // acceptance thresholds on |hz|
  double w[kLayers]{};  // layer widths / 2^31
  double f[kLayers]{};  // density at the layer edges

  ZigguratTables() {
    const double m1 = 2147483648.0;
    const double v = 9.91256303526217e-3;
    double dn = kR;
    double tn = dn;
    const double q = v / std::exp(-0.5 * dn * dn);
    k[0] = (dn / q) * m1;
    k[1] = 0.0;
    w[0] = q / m1;
    w[kLayers - 1] = dn / m1;
    f[0] = 1.0;
    f[kLayers - 1] = std::exp(-0.5 * dn * dn);
    for (int i = kLayers - 2; i >= 1; --i) {
      dn = std::sqrt(-2.0 * std::log(v / dn + std::exp(-0.5 * dn * dn)));
      k[i + 1] = (dn / tn) * m1;
      tn = dn;
      f[i] = std::exp(-0.5 * dn * dn);
      w[i] = dn / m1;
    }
  }

  static const ZigguratTables& Get() {
    static const ZigguratTables tables;
    return tables;
  }
};

/// Standard-normal, gamma and Student-t variates on top of one stream.
class Sampler {
 public:
  explicit Sampler(Xoshiro256pp rng) : rng_(rng), zig_(ZigguratTables::Get()) {}

  double Uniform() { return rng_.UniformOpen(); }

  /// Ziggurat standard normal. Layer index and magnitude come from disjoint bits of one
  /// 64-bit draw (avoids the bit-reuse flaw noted by Doornik 2005).
  double Normal() {
    for (;;) {
      const uint64_t bits = rng_.Next();
      const int layer = static_cast<int>(bits & 127U);
      const auto hz = static_cast<int32_t>(static_cast<uint32_t>(bits >> 32));
      const double x = static_cast<double>(hz) * zig_.w[layer];
      if (std::abs(static_cast<double>(hz)) < zig_.k[layer]) return x;
      if (layer == 0) {  // tail beyond R, by Marsaglia's exponential method
        double tx = 0.0;
        double ty = 0.0;
        do {
          tx = -std::log(Uniform()) / ZigguratTables::kR;
          ty = -std::log(Uniform());
        } while (ty + ty < tx * tx);
        return hz > 0 ? ZigguratTables::kR + tx : -ZigguratTables::kR - tx;
      }
      if (zig_.f[layer] + Uniform() * (zig_.f[layer - 1] - zig_.f[layer]) < std::exp(-0.5 * x * x)) {
        return x;
      }
    }
  }

  /// Gamma(shape, 1) by Marsaglia & Tsang (2000); shape < 1 uses the u^(1/shape) boost.
  double Gamma(double shape) {
    if (shape < 1.0) return Gamma(shape + 1.0) * std::pow(Uniform(), 1.0 / shape);
    const double d = shape - 1.0 / 3.0;
    const double c = 1.0 / std::sqrt(9.0 * d);
    for (;;) {
      const double x = Normal();
      double v = 1.0 + c * x;
      if (v <= 0.0) continue;
      v = v * v * v;
      const double u = Uniform();
      const double x2 = x * x;
      if (u < 1.0 - 0.0331 * x2 * x2) return d * v;
      if (std::log(u) < 0.5 * x2 + d * (1.0 - v + std::log(v))) return d * v;
    }
  }

  /// Student-t with `nu` degrees of freedom (unit scale, variance nu/(nu-2)).
  double StudentT(double nu) { return Normal() / std::sqrt(2.0 * Gamma(0.5 * nu) / nu); }

 private:
  Xoshiro256pp rng_;
  const ZigguratTables& zig_;
};

}  // namespace susq::sim
