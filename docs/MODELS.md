# Models: `sim`, `tourney`, `needle`

The three C++20 models behind Susquatch's strategy. The numbers they produce decide the scenario book
and the election-night engine, so this document states every equation, where each parameter comes from,
how the models are validated, and where they are knowingly wrong.

| Model | Question it answers | Runs as | Called by |
|---|---|---|---|
| `sim` | What joint outcomes are plausible, given real-money prices? | library | Python (`susq_models.sim`) once a day or on input changes |
| `tourney` | Given those worlds, how likely is a book to finish 1st? | library | Python (`susq_models.tourney`) in the optimizer |
| `needle` | On election night, where is each race heading given counted votes? | `susq-needle` service on NATS, plus a library | Rust `enr` → needle → Rust executor; Python replays |

Process boundaries follow `brief/LANGUAGES_AND_BOUNDARIES.md`: C++ talks to Python only through pybind11 and
to Rust only through NATS messages (`contracts/proto/susq/v1/bus.proto`). Nothing here is linked into Rust.

Notation: margins are **D-side minus R, in points of the two-party vote**. `n` is the number of draws,
`i` indexes races, `d` indexes draws.

---

## 1. `sim` — correlated race Monte Carlo

### 1.1 Model

For race `i` in state `s(i)`, region `r(i)` and draw `d`:

```
margin[i,d] = mu[i] + e[i,d]
e[i,d]      = beta[i] * N[d] + R[r(i),d] + S[s(i),d] + sum_c L[i,c] * C[c,d] + sigma[i] * eps[i,d]

N[d] = sigma_N * sqrt((nu - 2) / nu) * t_nu[d]  +  M[d]              national shift
M[d] = +m with prob p*q,  -m with prob p*(1-q),  0 otherwise         systematic polling miss
R ~ N(0, sigma_R^2) per region,  S ~ N(0, sigma_S^2) per state,  C[c] ~ N(0, sigma_c^2) per cluster
eps ~ N(0, 1) per race
```

`t_nu` is a unit Student-t, rescaled so that the non-miss part of `N` has SD exactly `sigma_N`. Then

```
Var(N)    = sigma_N^2 + p m^2 - (p (2q - 1) m)^2
Var(e[i]) = beta_i^2 Var(N) + sigma_R^2 + sigma_S^2 + sum_c L[i,c]^2 sigma_c^2 + sigma_i^2
Cov(e[i], e[j]) = beta_i beta_j Var(N) + 1{same region} sigma_R^2 + 1{same state} sigma_S^2
                  + sum_c L[i,c] L[j,c] sigma_c^2
```

Races in the same state share `S` (SPEC §5: "shared by Senate, Governor, and House races in the
state"), so same-state correlation is strictly higher than same-region, which is higher than cross-region.
A race with an empty `state` gets neither `R` nor `S`. That is how the research pipeline represents
chamber pseudo-races.

**Outcomes.** With probability `p_third_party[i]`, drawn independently of every shock, a candidate outside
the modeled margin wins and both party markets resolve NO. Otherwise the D-side candidate wins when
`margin > 0`. A race flagged `dem_side_is_independent` (Nebraska Senate: Osborn (I) vs Ricketts) reports
`P(Democratic market YES) = 0` even when its D side wins. `dem_side_caucuses_dem` controls whether that
win counts toward the Democratic caucus. Each draw stores one `uint8` per race (0 = R, 1 = D side,
2 = third party). That is the joint outcome matrix Python uses for scenario analysis.

### 1.2 Parameters and sources

| Parameter | Default | Source |
|---|---|---|
| `sigma_N` (national SD) | 2.75 | SPEC §5: 2.5–3.0 |
| `nu` (t degrees of freedom) | 5 | SPEC §5: 4–6. Fat tails for national misses |
| `p`, `m` (systematic miss) | 0.1, ±3.5 | SPEC §5: "with p ≈ 0.1, add an extra N shift of ±3–4" |
| `q` (P(miss favours D)) | 0.5 | Symmetric by default. RESEARCH §5 notes R beat August Senate polls 2018–2024, but "correcting for predicted poll bias helped in only 9 of 14 cycles", so direction stays an input, not a default |
| `sigma_R` (region) | 1.5 | SPEC §5. Regions are the census four {Northeast, Midwest, South, West}; the map is configurable |
| `sigma_S` (state) | 2.5 | SPEC §5 |
| `sigma_i` (race) | 3.0 | SPEC §5: 2.5–3.5, larger with sparse polls or independents |
| Cluster SDs | set per cluster | SPEC §5 clusters: red-state reach (TX, OH, IA, KS, AK, SC), New England incumbency (ME), Midwest industrial (MI, OH), House suburban |

At the defaults the total error SD of a race is `sqrt(7.56 + 1.23 + 2.25 + 6.25 + 9.0) ≈ 5.1` points
of margin. That sits inside SPEC §5's 4.5–5.5 and is consistent with RESEARCH §6: Shirani-Mehr et al.
(JASA 2018) find state-poll RMSE ≈ 3.5 points of *vote share* (≈ 7 points of margin, but that is before
averaging polls), and FiveThirtyEight's weighted-average absolute error on margin is 6.0 overall and
4.8 in 2021–22.

### 1.3 House

The House is driven by the generic ballot (RESEARCH §5: D+7.4 to D+8.1):

```
district margin[k,d] = (GB + LV) + N[d] + H[d] + lean[k] + incumbency[k]
                       + R[r(k),d] + S[s(k),d] + sum_c L[k,c] C[c,d] + sigma_district * eta[k,d]
```

`H ~ N(0, national_extra_sd^2)` is extra House-vote noise (default 0: `N` already carries national
polling error). `sigma_district` defaults to 5. With the region and state shocks, the idiosyncratic
district SD is `sqrt(25 + 8.5) ≈ 5.8`, inside SPEC §5's 5–7. A district whose id matches a traded race
(or that sets `race_id`) takes that race's calibrated outcome instead of an independent draw, so the
seat count and the district market can never disagree.

### 1.4 Chambers

- **Senate D seats** = `dem_seats_not_up` + number of Senate races where the D side wins and caucuses with D.
  Control = seats ≥ `control_threshold` (51), or seats = 50 with `vp_breaks_ties_dem`.
- **House D seats** = D wins over all districts. Control = seats ≥ 218.

Outputs are the per-draw seat counts, their PMFs, and `P(control)`.

### 1.5 Calibration to real-money prices

Given a target `P(D side wins)` per race (e.g. a Kalshi/Polymarket mid), `mu[i]` is solved **on the
realized draw set** with common random numbers:

1. `Generate` draws every mu-independent quantity once: `N`, shocks, `e[i,d]`, third-party events. It
   is deterministic in `seed`.
2. For each targeted race, let `E` be its errors on draws without a third-party win. We need
   `#{d : mu + e[d] > 0} = round(target * n)`. The count is a step function of `mu`. Its exact root is
   `mu = -q`, where `q` sits halfway between the two order statistics of `E` that bracket the required
   count. This is the fixed point bisection on `mu` would converge to. We find it by selection
   (`nth_element`, O(n)) rather than by iterating.
3. The realized marginal then matches the target to within `1/(2n)`. At 10⁵ draws that is 0.0005, well
   inside the 0.1-point requirement. The tests assert `< 0.001`.

Because every race is calibrated on the same worlds, the correlation structure is untouched. Only the
means move. Targets above `1 - realized third-party share` are infeasible, and the report flags them.

**Calibrating marginals does not calibrate chambers.** Chamber probabilities fall out of the correlation
structure. SPEC §5 asks for per-race *and* chamber probabilities within 2–4 points of real money. If the
chamber misses, suspect the correlation parameters first ("assume a bug before assuming edge").
RESEARCH §5's example: independent race prices imply 64% D Senate, while the control market says 62%.
That gap is correlation.

### 1.6 Scenario bundles (SPEC §3 Phase 2)

Draws are bucketed by **national-shift decile × signs of chosen cluster shocks**. With `k` clusters that
gives `10 · 2^k` buckets, where bucket = `decile · 2^k + Σ_c 1{C_c > 0} 2^c`. Each bucket carries its
probability, mean `N`, per-race conditional `P(D market)` and `P(R market)`, and conditional chamber
control. `draw_bucket` maps every draw to its bucket, so Python can join any other per-draw quantity.
By construction `Σ_b P(b) P(D | b) = P(D)`, and the tests assert it.

### 1.7 Determinism and performance

Every random quantity comes from a xoshiro256++ stream seeded by SplitMix64 from `(seed, entity key,
block)`. The entity key is a hash of e.g. `race:2026-SEN-OH-S`, and a block is 4,096 draws. Blocks are
shared among `std::jthread` workers dynamically, and each writes only its own slice. So:

- results are bit-identical for any thread count (tested for 1, 2, 3 and 8);
- adding or removing a race leaves every other race's draws unchanged (tested). Research comparisons
  between configurations therefore use common random numbers for free.

Normals come from a 128-layer ziggurat, whose layer index and magnitude use disjoint bits of one
64-bit draw. Gammas for the t use Marsaglia–Tsang.

`sim_bench` (1e6 draws, 60 races, 435 House districts, 4 clusters) on an 8-core Apple M-series machine
under load: generate 1.6 s, calibrate 0.25 s, evaluate 0.17 s, bundles 0.29 s, **2.3 s total**. With 60
races and no districts, generation is about 1.2 s. On one thread everything takes 9–12 s; the
2-vCPU server should take about 5–6 s. Memory is dominated by the float error matrix: 240 MB at
1e6 × 60.

---

## 2. `tourney` — field model and P(1st)

### 2.1 Field

Given a world `ω` (which contracts pay), each of the `N` other players is independently one of four types
(SPEC §6):

| Type | Share | Final multiple |
|---|---|---|
| passive | `f_passive` | exactly 1.0 (qualifying trade, then nothing) |
| consensus / Kelly | `f_kelly` | `Uniform[0.8, 1.3]` |
| YOLO | `f_yolo` | all-in on contract `j` with prob `π_j ∝ p_j^α` among contracts priced ≤ `yolo_max_price`; ends at `1/p_j` if `j` pays, else 0 |
| election-night bot | `f_bot` | `rolls` all-in rolls, each ×`r` with prob `s`, else ×`keep` |

`α = 1` is "price-weighted": a 10¢ longshot draws twice the players of a 5¢ one, and each contract's
expected contribution to the top of the leaderboard is then equal. Field size `N` is observable: it is
the leaderboard `total` (ERRATA E8).

For your final multiple `M_ω` in world `ω`, let `a_ω(M)` be the probability that one random player
finishes strictly above you and `τ_ω(M)` the probability of an exact tie (relative tolerance 1e-9).
Both are closed-form sums over the type mixture.

### 2.2 Prizes with the tie-split rule

Official Rules §3 (RESEARCH §1.3): $30k / $5k / $2.5k, **no tiebreakers**, and tied players share the
pooled prizes of the places they occupy. With `A` players strictly above you and `T` tied, you occupy
places `A+1 … A+1+T` and receive `(Σ prizes of those places) / (T+1)`. Given `ω`, the count `A` is
`Binomial(N, a)`, and given `A = k` the count `T` is `Binomial(N-k, τ/(1-a))`. Only `k < 3` can pay, and

```
E[1/(T+1)]  for T ~ Binomial(n, p)  =  (1 - (1-p)^(n+1)) / ((n+1) p)
```

makes the expectation exact in O(number of prizes) per world:

```
E[prize | ω] = Σ_{k<3} P(A=k) [ Σ_{t < 2-k} P(T=t) S(k,t)/(t+1)
                               + S_full(k) (E[1/(T+1)] - Σ_{t < 2-k} P(T=t)/(t+1)) ]
```

`P(1st)` uses the prize vector `(1)` and `P(top 3)` uses `(1,1,1)`. Both give fractional credit for
ties, as the spec asks. Everything is averaged over the sim's worlds with optional world weights.

### 2.3 Field maximum `T(ω)`

Players are i.i.d. given `ω`, so `P(T ≤ m | ω) = (1 - a_ω(m))^N`. `threshold_cdf` evaluates this
world-weighted CDF in one sorted sweep per world. `threshold_quantiles` inverts it exactly at every atom
(1, 0, each `1/p_j`, each bot value) and on a 1e-3 grid over the consensus range.

A structural consequence, visible in the example below: once `f_yolo · N` is large, `T` saturates at
`1/p` of the **cheapest contract that wins** in that world. That is SPEC §6's "capped by the cheapest
winning price". Beating it takes a correlated multi-leg book or election-night rolling, not a single leg.

Illustration only (not a forecast): the 11 Senate races of RESEARCH §5, calibrated as above, give 22 party
contracts at 0.5¢ ticks. Default field mix is 55% passive, 30% consensus, 10% YOLO, 5% bots.

| N | T 10% / 50% / 90% | all-in SC-D (8×): P(1st) / P(top 3) / E[prize] |
|---|---|---|
| 500 | 1.3 / 15.6 / 15.6 | 0.15% / 1.2% / $82 |
| 2,000 | 15.6 / 15.6 / 15.6 | ≈0 / 0.01% / $0.43 |

The real Cup has ~240 markets, including House seats and cheaper tails, so real thresholds are higher.
RESEARCH §4 gives priors of 8–15× at N≈500, 15–30× at N≈2,000 and 25–100× at N≈10,000.

### 2.4 Fitting the field to the leaderboard (ITERATION §2.2)

`fit_field` takes leaderboard snapshots. Each has `N`, the top-k balances as multiples, the value a YOLO
holding each contract would have at snapshot time (mark/entry before resolution, `1/p` or 0 after) and
the entry prices. The i-th largest of `N` i.i.d. draws sits near survival `(i - ½)/N`, so the fit
minimizes

```
Σ_snapshots Σ_i ( log(N · S(x_i)) - log(i - ½) )²,    S(x) = P(X > x) + ½ P(X = x)
```

over a grid of `f_yolo` (default 0–0.5 in steps of 0.005) and, optionally, `kelly_hi`. The passive,
consensus and bot shares keep their base proportions. The mid-survival makes atoms (everyone at 1.0 or
at `1/p_j`) well behaved. Tests recover `f_yolo = 0.12` to ±0.04 from synthetic leaderboards.

### 2.5 Sampled threshold (parity with the research reference)

`SampleThresholds` (Python: `susq_models.simulate_threshold`) draws one `T(ω)` per world with exactly
the semantics of `py/susq/models/tourney.simulate_threshold`, so the research facade can swap engines.
Those semantics are: fixed type counts `round(N·frac)` (half to even), maxima via `F⁻¹(U^{1/n})`,
lognormal bots, and every contract YOLO-eligible, with independent `Bernoulli(1 − (1 − w_j)^{n_yolo})`
picks. They differ from §2.1–2.3 on purpose. Use the exact model for P(1st) of a candidate book and the
sampled one where the research pipeline expects draws.

---

## 3. `needle` — election-night county model

### 3.1 Hierarchy

For vote type `v` (mail, early in person, Election Day, provisional; `MIXED` when a source doesn't split)
in county `c` of race `r`, the margin of **all** type-`v` ballots is

```
m[c,v] = p[c,v] + beta_r N + delta_r + eta_c + zeta[c,v]

p[c,v]      = county prior margin (aligned, §3.5) + vote-type margin offset          (NeedlePrior)
N           ~ N(0, national_shift_prior_sd^2)                     shared by all races
delta_r     ~ N(0, sigma_delta_r^2)                               race/state level, all counties of r
eta_c       ~ N(0, prior_margin_sd_c^2)                           county
zeta[c,v]   ~ N(0, type_sd^2)                                     county x vote type (default 3)
```

`delta_r` is the correlated county error within a state: every county of the race moves together.
Reported ballots of type `v` are an early subset. Their margin `y[c,v]` differs from `m[c,v]` by batch
noise with variance `within_type_sd² (1-f)/f`, where `f` is the reported fraction of that type. It
vanishes when the type is complete. (Equal batches with i.i.d. deviations that sum to zero give exactly
this form.)

### 3.2 Remaining votes

Expected type totals are `expected_total_votes × share_of_total`. With no progress information,
`remaining = max(expected - reported, 0)`. When the source reports precinct progress `ρ` (reporting /
total), the unit total is estimated as `ρ·(reported/ρ) + (1-ρ)·expected` (the source's own projection,
trusted in proportion to how much it has counted), and `remaining = (1-ρ) × that`. `ρ = 1` marks the
unit complete. Tallies that don't split by type (`MIXED`) are allocated to types in their
`typical_report_order` (mail first, …), so the uncounted remainder is weighted toward the late types.
Turnout uncertainty enters as SD `expected_total_sd_frac × expected × sqrt(remaining/expected)` on each
county's remaining count.

### 3.3 Exact Gaussian conditioning in O(counties)

Each recompute conditions the hierarchy on all stored cumulative results:

1. **County summaries.** Cells (reported types) share `eta_c`. With cell variance
   `t_v = type_sd² + batch noise`, Sherman–Morrison gives the county's information about the race-level
   deviation `D_r = beta_r N + delta_r`: the 1/t-weighted residual `z_c` with variance
   `sd_eta² + 1/Σ(1/t_v)`.
2. **Race summaries.** Each race gets a precision-weighted `z_r` with precision `P_r`, which grows with
   the votes counted.
3. **National shift.** `N | data` has precision `1/σ_N² + Σ_r β_r² / (σ_δr² + 1/P_r)` and the matching
   mean. That is the Bayesian update of `N` from each race's deviation versus its prior, weighted by how
   much of the race has reported. It is published as `needle.national_shift`.
4. **Race deviation.** With `k_r = σ_δr² / (σ_δr² + 1/P_r)`:
   `E[D_r] = β E[N] + k_r (z_r - β E[N])` and
   `Var(D_r) = β²(1-k_r)² Var(N) + σ_δr² (1-k_r)`. A race with no returns has `k_r = 0`, so it moves by
   `β_r E[N]`. This is how early East Coast counts move western races.
5. **County and type shrinkage.** `E[eta_c] = Σ_v (y_v - p_v - D)/t_v / (1/sd_eta² + Σ 1/t_v)`, then
   `E[zeta_v] = h_v (y_v - p_v - D - eta_c)` with `h_v = type_sd² / t_v`. This is the shrinkage
   estimator of each vote type's margin from what that type has already shown in that county.
6. **Race margin.** Reported votes are known. For each county, the uncounted votes contribute
   `Σ_v b_v m̂[c,v] - reported_v y_v`, with `b_v = reported + remaining` two-party votes of type `v`. Using
   the full-type margin (not just "the rest looks like the latent margin") keeps intervals honest while a
   type is partially counted. Each county's contribution is linear in `D` with coefficient
   `κ_c = (b - Σ b_v h_v)(1 - g_c)`, with `g_c` the county's eta gain, plus conditional variance `τ_c²`.
   Hence

   ```
   Var(race margin) = [ (Σ_c κ_c)² Var(D_r) + Σ_c τ_c² + turnout terms ] / W²
   ```

   exactly, where `W` is the race's total two-party votes. The correlation between `D` and each county's
   `eta` is accounted for rather than double counted.
7. `p_side = Φ(mean / sd)`. `p_rep = 1 - p_side`. For ordinary races `p_dem = p_side`. For
   `independent_major` races the non-R side pools DEM and IND votes and
   `p_dem = p_side × (DEM share of that side so far)` (0 before any DEM votes). Mass is left for the
   independent, as the contract allows (`p_dem + p_rep ≤ 1`).

A recompute over 3,000 counties × 3 vote types takes **0.8 ms** (median, RelWithDebInfo). The spec
budget is 10 ms.

### 3.4 Validation on synthetic election nights

`needle/tests/synthetic.hpp` draws a 40-county state from the model's own generative process. It reports
four equal batches per county and vote type in a realistic order: mail first, then early vote, then
Election Day last, with random county interleaving. Over 300 simulated nights:

| Reported | 80% interval coverage | 95% interval coverage | mean z² |
|---|---|---|---|
| 0% (prior) | 0.797 | 0.947 | 0.99 |
| ≥25% | 0.790 | 0.947 | 1.06 |
| ≥60% | 0.797 | 0.953 | 1.00 |

When everything is counted, posterior means match the true final margin to within 0.05 points. At 30%
reported, while the count is still mail-heavy and D-skewed, the model's absolute error is under half the
naive running tally's.

### 3.5 Prior alignment

County priors are shifted uniformly so their turnout-weighted average, including vote-type offsets,
equals the race's `prior_margin_mean`. `sigma_delta_r²` is set so the prior SD of the race margin equals
`prior_margin_sd`: race variance minus `β² σ_N²`, minus aggregated county/type variance, minus turnout
variance. It is floored at `race_level_sd_floor²`, and when the floor binds the no-data SD comes out a
little above the prior (the golden fixture is one such case: prior 5.0, model 5.43).

### 3.6 Ingestion rules

- Results are cumulative per `(race, county, vote type, source)`. The model keeps each source's latest
  totals and uses the most complete source per unit. Cross-source consistency is the Rust `enr`
  adapters' job.
- `batch_id` dedupe: a seen id is ignored. Identical totals under a new id count as `unchanged`.
- Regressions (any cumulative count going down) are ignored and counted **unless flagged as
  corrections**. The `susq.v1.CountyResult` contract has no correction flag yet, so the bus path never
  accepts one. The library call `Ingest(result, is_correction)` and the Python `ingest(...,
  is_correction=True)` can.
- Malformed results are rejected and counted: no candidate votes, a total below the candidate sum, any
  count above 5e7, or an unknown vote-type enum. Unknown races are counted too. Counties missing from the
  prior are counted toward reported votes but not modeled.

### 3.7 The `susq-needle` service

```
susq-needle --prior PRIOR.json [--nats nats://127.0.0.1:4222] [--heartbeat-ms 1000]
susq-needle --prior PRIOR.json --replay BATCHES.jsonl [--out OUT.jsonl]
```

| Subject | Direction | Payload | When |
|---|---|---|---|
| `enr.county_results` | in | `CountyResultsBatch` (binary) | from Rust `enr` |
| `needle.race_posteriors` | out | `RacePosteriors` | after every accepted batch |
| `needle.national_shift` | out | `NationalShift` | after every accepted batch |
| `needle.heartbeat` | out | `Heartbeat` (`status`, `last_input_seq`) | every `--heartbeat-ms` (≤ 2 s enforced) |

- Every output carries a `Header`: `schema_version`, `produced_at_ns`, and producer `needle` plus the git
  SHA resolved at CMake configure time. `seq` is strictly increasing **per subject**. It starts at
  wall-clock microseconds, so it keeps increasing across restarts; the downstream watchdog sees a restart
  as a gap and re-qualifies needle after N healthy messages (`config/susq.toml [needle]`).
- Inputs without a header or with a foreign `schema_version` are dropped (`bad_header`), as are
  undecodable payloads (`malformed`). For 10 s after a drop the heartbeat says
  `degraded:rejected_inputs`. While the bus is down it says `degraded:bus_disconnected`. Otherwise `ok`.
- NATS: connects in the background and reconnects forever (500 ms wait). The integration test restarts
  `nats-server` mid-run and requires a fresh round trip afterward. SIGTERM/SIGINT flush, unsubscribe and
  exit 0.
- `--replay` reads one proto3-JSON `CountyResultsBatch` per line (malformed lines are skipped and logged).
  It writes `{"subject": ..., "payload": {...}}` lines, uses the batch's own `produced_at_ns` so outputs
  are reproducible, and never touches the network.

---

## 4. Calling the models from Python

```bash
uv sync --group models          # builds cpp/ with scikit-build-core + pybind11 (≈1–2 min)
uv run --group models python    # plain `uv run` re-syncs without the group and removes it
```

```python
import numpy as np
from susq_models import sim, tourney, needle

cfg = sim.default_config()  # census regions, SPEC §5 shocks
cfg.n_draws = 500_000
cfg.clusters = [sim.ClusterSpec(name="red-state-reach", sd=2.0)]
cfg.races = [  # assign whole lists (fields are copied on access)
    sim.RaceSpec(
        id="2026-SEN-OH-S",
        state="OH",
        chamber=sim.Chamber.SENATE,
        target_p_win=0.58,
        clusters=[sim.ClusterLoading(cluster="red-state-reach", loading=1.0)],
    ),
    sim.RaceSpec(
        id="2026-SEN-NE",
        state="NE",
        chamber=sim.Chamber.SENATE,
        target_p_win=0.45,
        dem_side_is_independent=True,
        dem_side_caucuses_dem=False,
    ),
]
cfg.senate = sim.SenateSpec(dem_seats_not_up=34)
model = sim.Model(cfg)
model.generate()
report = model.calibrate()
model.evaluate()  # or model.run()
model.outcomes  # (n_races, n_draws) uint8
model.p_dem_market, model.p_senate_dem_control
bundles = model.scenario_bundles(["red-state-reach"])  # dict of numpy arrays

pays = np.vstack([model.outcomes == sim.DEM_SIDE_WINS, model.outcomes == sim.REP_WINS]).astype(np.uint8)
prices = np.concatenate([model.p_dem_market, model.p_rep_market]).clip(0.005, 0.995)
field = tourney.FieldModel(prices, pays, tourney.FieldParams(n_players=2000))
tourney.evaluate_book(field, cash=0.0, contracts=[0], shares=[100_000 / prices[0]])
tourney.threshold_quantiles(field, [0.5, 0.9])

nm = needle.NeedleModel(open("prior.json").read())
nm.ingest_batch_bytes(raw_batch)  # or ingest_batch(dict) / ingest(dict); then read:
nm.races(), nm.national(), nm.counties()
```

Two narrow, versioned surfaces exist for other components:

- **Research facade** (`py/susq/models/facade.py`, `docs/RESEARCH.md`): the top-level module exposes
  `SIM_API_VERSION = 1` with `simulate_margins(...)`, returning `(margins[n,k], national[n],
  clusters[n,c])` with region/state index −1 meaning none, and `TOURNEY_API_VERSION = 1` with
  `simulate_threshold(...)`. When `susq_models` is installed, `facade.get_engine().name ==
  "susq_models"` (tested).
- **Replay tooling** (`py/susq/replay/backends.py`): `needle.Needle()` with `load_prior(bytes)` for a
  binary `NeedlePrior` and `ingest(bytes) -> bytes` taking a `CountyResultsBatch` and returning
  `RacePosteriors`. It runs the service core itself: the same header validation and the same output
  headers and per-subject `seq`, stamped with the batch's own `produced_at_ns`. Undecodable or
  foreign-schema batches raise `ValueError` and are counted.

`cpp/tests_py/` has worked examples. It includes `reference_sim.py`, a pure-numpy re-implementation of
the sim's structure used as an oracle: marginals agree within Monte Carlo error, and the empirical error
covariance matches the closed form in §1.1.

---

## 5. Known limitations

**sim**
- Shocks are Gaussian except the national t. SDs are fixed inputs, not fitted to polling history, and
  there is no time dimension (no drift between now and Nov 3).
- Third-party wins are independent of every shock. A competitive independent's chances do not move with
  the national environment.
- Each race's marginal is calibrated to its own market. Nothing forces chamber probabilities to match
  chamber markets; compare them by hand (§1.5).
- House districts need leans and incumbency inputs from Python. The binary D/R district model has no
  third parties or top-two same-party races (California).
- RCV (Alaska, Maine) is not modeled. Those races are a plain margin.

**tourney**
- Players are independent given the world. YOLOs pick one contract. Consensus players and bots do not
  depend on the world. Real players herd (RESEARCH §5 "field-exposure hypothesis"); the field-exposure
  term in scenario selection has to come from Cup-vs-Kalshi drift, not from this model.
- Your own trading does not move prices in the model.
- The fit is a grid search on quantile matching. Before resolution it needs mark/entry values per
  contract, which only leaderboard telemetry plus price history can supply.

**needle**
- Gaussian throughout. Posterior `p_dem` is `Φ(mean/sd)`, with no fat tails for "whole county missing"
  errors.
- `delta_r` is per race. A Senate race and a Governor race in the same state are correlated only through
  `N`, and neighbouring states only through `N`.
- `independent_major` handling is a heuristic (IND votes join the non-R side). It fits Nebraska, not a
  genuine three-way race.
- Precinct progress is read per unit as reported. Feeds whose precinct counts cover Election Day only
  will overstate mail completeness.
- Corrections cannot arrive over the bus until `CountyResult` gets a correction flag (see the report).
- The git SHA in headers is resolved when CMake configures, so rebuilding without reconfiguring keeps the
  old SHA.
