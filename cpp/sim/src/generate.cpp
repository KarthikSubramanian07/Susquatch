#include <algorithm>
#include <cmath>
#include <vector>

#include "layout.hpp"
#include "susq/sim/parallel.hpp"
#include "susq/sim/rng.hpp"
#include "susq/sim/simulator.hpp"

namespace susq::sim {
namespace {

// Fills out[0..n) with sd * N(0,1) from the entity's stream for this block.
void FillNormals(uint64_t seed, const std::string& key, std::size_t block, double sd,
                 std::vector<double>& out, std::size_t n) {
  Sampler s(Xoshiro256pp(seed, HashKey(key), block));
  for (std::size_t k = 0; k < n; ++k) out[k] = sd * s.Normal();
}

// Region + state shock of an entity for this block (zeros when it has no state).
const std::vector<double>& GeoShocks(int state_index, const detail::Layout& layout,
                                     const std::vector<std::vector<double>>& region,
                                     const std::vector<std::vector<double>>& state, std::vector<double>& scratch) {
  if (state_index < 0) {
    std::fill(scratch.begin(), scratch.end(), 0.0);
    return scratch;
  }
  const auto st = static_cast<std::size_t>(state_index);
  const auto& reg = region[static_cast<std::size_t>(layout.state_region[st])];
  for (std::size_t k = 0; k < scratch.size(); ++k) scratch[k] = reg[k] + state[st][k];
  return scratch;
}

double ClusterTerm(const detail::Loadings& loadings, const std::vector<std::vector<double>>& shocks,
                   std::size_t k) {
  double sum = 0.0;
  for (const auto& [c, w] : loadings) sum += w * shocks[static_cast<std::size_t>(c)][k];
  return sum;
}

}  // namespace

WorldSet Generate(const SimConfig& config) {
  const detail::Layout layout = detail::BuildLayout(config);
  const std::size_t n = config.n_draws;
  const std::size_t n_races = config.races.size();
  const std::size_t n_clusters = config.clusters.size();
  const ShockParams& sh = config.shocks;

  WorldSet w;
  w.n_draws = n;
  w.national.resize(n);
  w.cluster_shocks.resize(n_clusters * n);
  w.race_errors.resize(n_races * n);
  w.third_party.resize(n_races);
  for (std::size_t i = 0; i < n_races; ++i) {
    if (config.races[i].p_third_party > 0.0) w.third_party[i].assign(n, 0);
  }
  std::vector<std::size_t> unlinked;
  for (std::size_t j = 0; j < config.districts.size(); ++j) {
    if (layout.district_race[j] < 0) unlinked.push_back(j);
  }
  if (!config.districts.empty()) w.house_unlinked_dem.assign(n, 0);

  // Unit-variance scaling of the t so that SD(N without misses) == national_sd.
  const double t_scale = sh.national_sd * std::sqrt((sh.national_nu - 2.0) / sh.national_nu);
  const double house_base = config.house.generic_ballot + config.house.lv_adjustment;
  const uint64_t seed = config.seed;
  const std::size_t n_blocks = (n + kBlockSize - 1) / kBlockSize;

  ParallelFor(n_blocks, config.threads, [&](std::size_t b) {
    const std::size_t begin = b * kBlockSize;
    const std::size_t len = std::min(kBlockSize, n - begin);

    std::vector<double> national(len);
    {
      Sampler s(Xoshiro256pp(seed, HashKey("national"), b));
      for (std::size_t k = 0; k < len; ++k) {
        double v = t_scale * s.StudentT(sh.national_nu);
        const double u_miss = s.Uniform();
        const double u_sign = s.Uniform();
        if (u_miss < sh.miss_prob) v += (u_sign < sh.miss_prob_dem ? sh.miss_size : -sh.miss_size);
        national[k] = v;
        w.national[begin + k] = static_cast<float>(v);
      }
    }
    std::vector<std::vector<double>> region(config.regions.size(), std::vector<double>(len));
    for (std::size_t r = 0; r < config.regions.size(); ++r) {
      FillNormals(seed, "region:" + config.regions[r], b, sh.region_sd, region[r], len);
    }
    std::vector<std::vector<double>> state(layout.states.size(), std::vector<double>(len));
    for (std::size_t s = 0; s < layout.states.size(); ++s) {
      FillNormals(seed, "state:" + layout.states[s], b, sh.state_sd, state[s], len);
    }
    std::vector<double> geo_scratch(len);
    std::vector<std::vector<double>> cluster(n_clusters, std::vector<double>(len));
    for (std::size_t c = 0; c < n_clusters; ++c) {
      FillNormals(seed, "cluster:" + config.clusters[c].name, b, config.clusters[c].sd, cluster[c], len);
      for (std::size_t k = 0; k < len; ++k) {
        w.cluster_shocks[c * n + begin + k] = static_cast<float>(cluster[c][k]);
      }
    }

    for (std::size_t i = 0; i < n_races; ++i) {
      const RaceSpec& race = config.races[i];
      const std::vector<double>& geo = GeoShocks(layout.race_state[i], layout, region, state, geo_scratch);
      Sampler s(Xoshiro256pp(seed, HashKey("race:" + race.id), b));
      float* out = &w.race_errors[i * n + begin];
      for (std::size_t k = 0; k < len; ++k) {
        const double e = race.national_beta * national[k] + geo[k] +
                         ClusterTerm(layout.race_clusters[i], cluster, k) + race.sd * s.Normal();
        out[k] = static_cast<float>(e);
      }
      if (race.p_third_party > 0.0) {
        Sampler t(Xoshiro256pp(seed, HashKey("third:" + race.id), b));
        for (std::size_t k = 0; k < len; ++k) {
          w.third_party[i][begin + k] = t.Uniform() < race.p_third_party ? 1 : 0;
        }
      }
    }

    if (!unlinked.empty()) {
      std::vector<double> house_national(len);
      FillNormals(seed, "house-national", b, config.house.national_extra_sd, house_national, len);
      std::vector<uint16_t> dem(len, 0);
      for (std::size_t j : unlinked) {
        const HouseDistrictSpec& d = config.districts[j];
        const std::vector<double>& geo = GeoShocks(layout.district_state[j], layout, region, state, geo_scratch);
        Sampler s(Xoshiro256pp(seed, HashKey("district:" + d.id), b));
        const double fixed = house_base + d.lean + d.incumbency;
        for (std::size_t k = 0; k < len; ++k) {
          const double margin = fixed + national[k] + house_national[k] + geo[k] +
                                ClusterTerm(layout.district_clusters[j], cluster, k) +
                                config.house.district_sd * s.Normal();
          if (margin > 0.0) ++dem[k];
        }
      }
      std::copy(dem.begin(), dem.end(), w.house_unlinked_dem.begin() + static_cast<std::ptrdiff_t>(begin));
    }
  });
  return w;
}

}  // namespace susq::sim
