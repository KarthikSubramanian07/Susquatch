<div align="center">

<img src="docs/img/logo.svg" width="96" alt="Susquatch logo: a trophy with a bell curve in the cup">

# Susquatch

**A large, elusive trading system that mostly sits very still in the woods,
then comes out on election night.**

An automated competitor for the [Susquehanna Predictions Cup](https://predictionscup.com/rules/): the 2026 U.S. midterm
elections, traded on [The Super Market](https://sig.thesuper.market/) with 100,000 play-money SUSQies.

`rust` · `c++20` · `python` · `prediction-markets` · `monte-carlo` · `election-modeling` · `market-microstructure` · `tournament-theory`

[![ci](https://github.com/KarthikSubramanian07/Susquatch/actions/workflows/ci.yml/badge.svg)](https://github.com/KarthikSubramanian07/Susquatch/actions/workflows/ci.yml)
![license](https://img.shields.io/badge/license-MIT-a1a1aa?style=flat-square)
![cost](https://img.shields.io/badge/infra%20cost-%240%2Fmonth-d97757?style=flat-square)

</div>

---

## The thesis, in one breath

The Cup pays **$30,000 to first place and very little to anyone else**. The rank is the balance *after every market resolves*,
so the only thing that finally counts is **what you hold at the lock (Nov 4, 12:00 ET)** and whether the world agrees with it.

That changes the game:

- **Expected value is the wrong target.** A sensible Kelly bettor finishes a comfortable 400th. Someone in the field
  will YOLO into ten correlated longshots and hit, and the winning multiple is probably **10–30×**.
- **Direction is not an edge.** SIG runs an 80-person prediction desk and quotes around Kalshi. Susquatch does not
  think it knows who wins Maine better than they do.
- **Latency and under-exposure are edges.** Stale student orders, constraint violations the platform itself
  broadcasts, and election-night county results that arrive minutes before a play-money book reprices.

So Susquatch does three things, in order:

| Phase | When | What | Why |
|---|---|---|---|
| 🌲 **Forage** | Oct 1 → ~Oct 20 | constraints racer · stale-order sniper · (maybe) market making | build capital, measure the field, calibrate |
| 🧭 **Stalk** | ~Oct 21 → Nov 2 | a **correlated scenario book**: cheap legs that all pay in one coherent world the field is under-exposed to | a multiple ≥ the winning threshold *T* |
| 🌕 **Emerge** | Nov 3 19:00 → Nov 4 11:55 ET | **election-night engine**: county results → `needle` → posteriors → buy what the data already knows, roll gains west | compounding 2–3× rolls is how 15× becomes 40× |

## Architecture

Three languages, **never linked together**. They talk only through versioned files and a message bus, and one schema
generates the types for all of them.

```
 Kalshi · Polymarket (read-only)                          Super Market (REST + Supabase Realtime)
          │                                                              ▲
          ▼                                                              │ orders (lease holder only)
 ┌─ python-scheduler ─────────────────┐   targets-<ts>-<hash>.json   ┌───┴──────────── rust-live ─────────────┐
 │ collectors · calibrate · optimizer │ ───────────────────────────► │ platform · gateway · book · oms · risk │
 │   └─ sim, tourney (C++ ⇄ pybind11) │   (schema + hash + max-age)  │ strategies · journal · targets         │
 └────────────────────────────────────┘                              │ enr (SoS / Clarity pollers)            │
                                                                     └───┬──────────────────────▲─────────────┘
                                             NATS  enr.county_results   │                      │ needle.race_posteriors
                                                                        ▼                      │ needle.national_shift
                                                                  ┌─ needle (C++) ─────────────┴┐   needle.heartbeat
                                                                  │ county expected-vote model  │
                                                                  └─────────────────────────────┘
      Cloudflare (free): Pages dashboard (auth + Kill) · KV status · cron Worker daily summary · R2 backups
```

| Layer | Language | Why this language |
|---|---|---|
| Everything that touches the exchange | **Rust** (tokio) | It must never crash, never double-send, and never trade on stale inputs |
| `sim`, `tourney`, `needle` | **C++20** | 10⁶ correlated draws in seconds, a county model that runs every 250 ms on election night |
| Research, collectors, calibration, reports | **Python** | Fastest path from a question to a chart |
| Oct 1 bridge | **Python** | Trading opens before the Rust stack has 24 h of paper time, so a tiny leased bridge covers day one |

## Safety model (why a bot with no pager is fine)

There are **no real-time alerts** by design. The system has to fail safe on its own:

- **Paper by default.** Live mode needs `SUSQ_MODE=live` *and* a human-written `ops/GO_LIVE` file.
- **Every order carries a GTD expiry** capped at the lock minus five minutes. If the process dies, its orders die too.
- **Deterministic idempotency keys** are written to disk *before* sending, and retries reuse them only for the codes the API documents as safe.
- **A single-trader lease.** The bridge and the Rust system can never both place orders.
- **Rust refuses stale inputs.** Old targets, a silent `needle`, a seq gap, or a stale fair value mean it stops adding exposure and cancels what those inputs placed.
- **Kill it from anywhere:** the `ops/KILL` file, the dashboard's Kill button, or NATS `ops.kill`.
- **Hard time gate:** no new orders at or after 11:55 ET on Nov 4.

## The dashboard, and the one message a day

<p align="center">
  <img src="docs/img/dashboard-desktop.png" alt="Susquatch dashboard on desktop" width="68%">
  &nbsp;
  <img src="docs/img/dashboard-mobile.png" alt="Susquatch dashboard on a phone" width="24%">
  <br><sub><i>Rendered from a synthetic status fixture. Every number here is demo data, not results.</i></sub>
</p>

A password-protected Cloudflare Pages site at `susquatch.pages.dev`: mode, stage, lease, heartbeat, equity, rank, health,
positions, trades, events and a two-step **Kill**. If the server goes quiet, the page turns clay and says since when.
Once a day a cron Worker, running on Cloudflare rather than the server, writes the summary (ACTION NEEDED first, usually
empty) to the dashboard and to a pinned GitHub issue, so a dead server still gets reported. All of it costs $0, and
`make costs` fails the build if that ever stops being true. See [`docs/OPS.md`](docs/OPS.md) and
[`docs/RUNBOOK.md`](docs/RUNBOOK.md).

## Rules we play by

One account. No coordination, and no trading against friends. No wash trades. No MNPI. The Sponsor owns all Competition Data,
so **raw platform logs never leave the private server**: `journal/`, `data/`, `reports/`, and `state/` are gitignored and
blocked by a pre-commit hook. The repo stays private until the lock. After that it publishes code, methodology, the simulator,
external-data backtests, and aggregate results only.

## Repo map

| Path | What lives there |
|---|---|
| [`brief/`](brief/) | The original spec and research, plus [`ERRATA.md`](brief/ERRATA.md): where reality disagreed |
| [`docs/API.md`](docs/API.md) | Our reading of the official API: what we call, how we retry, the lock-time arithmetic |
| [`PLAN.md`](PLAN.md) | Build order, assumptions to verify, pre-mortem, risk policy, human steps |
| `contracts/` | The single protobuf schema, golden fixtures, cross-language round-trip tests |
| `crates/` | The Rust live system (13 crates, one binary: `susq-live`) |
| `cpp/` | `sim`, `tourney`, `needle` (C++20) and their pybind11 bindings |
| `py/susq/` | Collectors, calibration, optimizer, replays, reports, platform client, mock exchange |
| `bridge_py/` | The Oct 1 bridge, archived after cutover |
| `ops/` | Compose, systemd, cloud-init, one-command restore, backups, `make costs` |
| `dashboard/`, `worker/` | Cloudflare Pages dashboard and the daily-summary cron Worker |
| `experiments/` | Every live change, with its hypothesis written down first |
| [`research_log.md`](research_log.md) | Timestamped notes that feed the write-up |

## Quickstart

```bash
make setup        # uv sync + cargo fetch + git hooks
make test         # Rust + Python tests (never touch the live API)
make help         # everything else
```

## Status

Built against a calendar that doesn't move: trading opens **Oct 1, 2026 12:00 ET**. [`PLAN.md`](PLAN.md) tracks
what's live, what's paper, and what's still in the woods.

*As of 2026-09-29.* Everything below is built, merged, and tested **against a faithful mock exchange**. Nothing has
touched the real API yet, because that needs a participant key, and the whole system defaults to paper mode until a human says "go live".

| Piece | State | Tests |
|---|---|---|
| Contracts (protobuf → Rust / C++ / Python) | ✅ byte-identical golden round-trips | 18 + 5 + 17 |
| Python platform client + mock exchange + paper broker | ✅ | part of 609 Python |
| Oct 1 bridge (gateway, OMS, risk, lease, racer, sniper, `measure-day1`, first trade) | ✅ paper-verified | part of 609 Python |
| `rust-live` (13 crates, `susq-live`) | ✅ paper-verified, also against the Python mock | 416 Rust |
| `sim` · `tourney` · `needle` (C++20) + `susq_models` | ✅ calibrated, coverage-checked | 101 C++ |
| Research: collectors, fair value, calibration, optimizer, telemetry, reports | ✅ live against public Kalshi/Polymarket | part of 609 Python |
| Election night: `enr` pollers, replays, poll-close table, chaos drills | ✅ captured the real CA SoS 2026 test feed | part of 416 Rust / 609 Python |
| Ops (Compose, backups, restore, self-check, `make costs`) | ✅ drills pass locally | 28 + 19 drill checks |
| Dashboard + daily-summary Worker | ✅ built, **not yet deployed** | 52 + 18 e2e |

The first real calibration landed without tuning: Senate control implied by the calibrated race marginals came out at
**59.2–59.5 %**, against **62 %** on real-money markets. That is within the 2–4 point target from the spec.

## License

[MIT](LICENSE). The code is ours. Competition Data is the Sponsor's, and none of it is in this repo.
