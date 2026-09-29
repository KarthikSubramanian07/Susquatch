//! Price in pips, sides, ids, time gates shared by every live crate.
//!
//! Everything here is pure (no I/O except [`config::Config::load`]) so it can be used from strategies,
//! risk checks and property tests without a runtime.

pub mod calendar;
pub mod clock;
pub mod config;
pub mod ids;
pub mod order;
pub mod price;
pub mod secret;
pub mod side;

pub use calendar::{Calendar, TradingWindow};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{Config, ConfigError, Mode};
pub use ids::{ExchangeId, MarketId, OrderId, Qty, TournamentId};
pub use order::{OrderBundle, OrderSpec};
pub use price::{Pips, PriceError};
pub use secret::Secret;
pub use side::{Action, BookSide, Side};
