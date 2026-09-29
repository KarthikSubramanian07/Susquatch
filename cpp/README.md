# Susquatch C++ models

`sim` · `tourney` · `needle` · pybind11 `susq_models` — C++20, never linked with Rust.

These three models sit behind Susquatch's strategy:

- **`sim`** — correlated race Monte Carlo. Student-t national shift with a systematic-miss mixture,
  region/state/cluster shocks, calibrated to real-money prices on common random numbers, plus Senate
  and House aggregation and scenario bundles.
- **`tourney`** — the competitor field. Given the sim's worlds, it gives closed-form P(1st), P(top 3)
  and expected prize for a book under the Official Rules' tie-pooling, the distribution of the field
  maximum, and a fit of the field mix to leaderboard snapshots.
- **`needle`** — the election-night county model: national shift → race → county → vote type, exact
  Gaussian conditioning in O(counties). It runs as the `susq-needle` NATS service and is also exposed to
  Python for replays.

The math, parameter sources, calibration, validation results and known limitations are in
[`docs/MODELS.md`](../docs/MODELS.md).

## Layout

```
cpp/
  CMakeLists.txt, cmake/deps.cmake   project, protobuf, GoogleTest, cnats, warning flags
  contracts/                         generated susq.v1 types (single schema in /contracts)
  sim/        include/susq/sim/      config.hpp, simulator.hpp, rng.hpp, parallel.hpp
              src/ tests/ bench/sim_bench.cpp
  tourney/    include/susq/tourney/  field.hpp, tourney.hpp, fit.hpp
              src/ tests/
  needle/     include/susq/needle/   model.hpp, service.hpp
              src/main.cpp, nats_bus.*  susq-needle executable
              tests/                 model, synthetic-night calibration, service, NATS integration
  bindings/                          pybind11 module susq_models (sim, tourney, needle submodules)
  pyproject.toml                     scikit-build-core build of susq_models
  tests_py/                          pytest suite, including a pure-numpy oracle for sim
```

## Build and test

Requirements: CMake ≥ 3.25, a C++20 compiler, and protobuf (`brew install protobuf` /
`apt install libprotobuf-dev protobuf-compiler`). GoogleTest is found or fetched. The NATS C client is
taken from an installed `cnats` (`brew install cnats`, which needs OpenSSL) or built from source with TLS
off.

```bash
make test-cpp                        # configure, build, ctest (all C++ tests)
./build/cpp/sim/sim_bench            # 1e6 draws x 60 races x 435 districts; args: draws threads races districts
make models                          # build susq_models via uv + run cpp/tests_py
```

The needle integration test starts a real `nats-server` if it can find one (`/opt/homebrew/bin`,
`/usr/local/bin`, `$PATH`, or `SUSQ_NATS_SERVER`). Otherwise it skips with a message.

CMake options: `SUSQ_BUILD_TESTS` (ON), `SUSQ_BUILD_NEEDLE_SERVICE` (ON; needs cnats),
`SUSQ_BUILD_PYTHON` (OFF; the pyproject turns it on).

## Python

`susq-models` is an opt-in dependency group of the root uv project, so a plain `uv sync` never compiles
C++:

```bash
uv sync --group models
uv run --group models python -c "from susq_models import sim; print(sim.default_config().regions)"
```

Standalone: `uv pip install ./cpp`. uv rebuilds when any `*.cpp`, `*.hpp`, CMake file or `.proto`
changes (`[tool.uv] cache-keys`).

## `susq-needle`

```bash
susq-needle --prior state/needle_prior.json --nats nats://127.0.0.1:4222      # live
susq-needle --prior prior.json --replay batches.jsonl --out posteriors.jsonl   # offline
```

It subscribes to `enr.county_results` and publishes `needle.race_posteriors`, `needle.national_shift` and
a `needle.heartbeat` every second, each with a `Header` and a strictly increasing per-subject `seq`.
Malformed or foreign-schema input is dropped and counted, and the heartbeat reports it. The service
reconnects forever and exits 0 on SIGTERM. See `docs/MODELS.md` §3.7.

## License

MIT, as for the rest of the repository.
