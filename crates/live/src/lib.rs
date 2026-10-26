//! `susq-live`: the only Rust process that talks to the exchange.
//!
//! The binary (`src/main.rs`) is a thin CLI over this library so the whole loop can be driven in
//! tests against the mock platform with a manual clock:
//!
//! * [`wiring::build`] assembles gateway, OMS, paper broker and [`engine::Engine`].
//! * [`engine::Engine::step`] is one tick of the trading loop.
//! * [`commands`] implements `run`, `paper-run`, `smoke`, `cancel-all` and `lease`.
//! * [`election`] runs the results pollers and publishes county results with `--election`;
//!   [`enr_health`] turns their health into the executor's disputed / stale-feed gates.
//! * [`realtime`] manages the capped set of per-market realtime channels.
//! * [`status`] writes `state/status.json`; [`http`] serves `/healthz` and `/metrics` on loopback.

pub mod cash;
pub mod cli;
pub mod commands;
pub mod election;
pub mod engine;
pub mod enr_health;
pub mod http;
pub mod realtime;
pub mod runtime;
pub mod status;
pub mod wiring;
pub mod world;
