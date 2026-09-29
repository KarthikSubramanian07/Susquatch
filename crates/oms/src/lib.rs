//! Order state machine, deterministic idempotency, write-ahead intents, reconcile.
//!
//! * [`idempotency`]: the cross-language key derivation (`docs/RUNTIME.md` rule 4).
//! * [`store`]: `state/rust-live/intents.jsonl`, fsynced before every send.
//! * [`state`]: the order lifecycle.
//! * [`oms`]: [`Oms`], which ties them together and reconciles against the venue.
//! * [`positions`]: signed positions from REST plus optimistic fills.

pub mod idempotency;
pub mod oms;
pub mod positions;
pub mod state;
pub mod store;

pub use oms::{FillDelta, LegMeta, Oms, OmsConfig, PrepareError, Prepared, ReconcileReport, Tracked};
pub use positions::PositionTracker;
pub use state::OrderState;
