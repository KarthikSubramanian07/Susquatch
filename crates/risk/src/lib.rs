//! Caps per market/cluster/gross, loss-per-hour breaker, locked legs, time gate, kill.
//!
//! * [`limits`]: [`PreTrade::check_bundle`], the pure pre-trade check every order passes through in
//!   the gateway. Property-tested: no accepted order ever leaves a cap breached.
//! * [`breakers`]: loss per hour, reject rate and bus error spikes, which freeze trading and ask
//!   for one cancel-all.
//!
//! Kill switches are enforced by the gateway (they must work even if risk state is corrupt).

pub mod breakers;
pub mod limits;

pub use breakers::{BreakerConfig, BreakerKind, Breakers, Trip};
pub use limits::{LiveOrder, PreTrade, Rejection, RiskContext, RiskLimits};
