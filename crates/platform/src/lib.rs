//! PlatformAdapter trait + SuperMarket REST/Realtime impl + in-memory Mock.
//!
//! * [`PlatformAdapter`]: exactly the endpoints in `docs/API.md`, one attempt per call.
//! * [`SuperMarket`]: the real REST client (reqwest), journaling every request/response.
//! * [`error`]: typed [`ApiError`] and the documented [`RetryClass`]es.
//! * [`models`]: tolerant wire models.

pub mod adapter;
pub mod error;
pub mod mock;
pub mod models;
pub mod realtime;
pub mod supermarket;

pub use adapter::{PlatformAdapter, all_markets, all_orders, all_relationships, bulk_prices};
pub use error::{ApiError, PlatformError, PlatformResult, REQUEST_IN_FLIGHT_WAIT, RetryClass};
pub use models::*;
pub use supermarket::{SuperMarket, SuperMarketConfig};
