//! Outbound integrations: destination adapters and source callbacks.
//!
//! Stage 1 ships two destinations — [`overlord`] submission and local export.
//! Every attempt is recorded on a `deliveries` row with a per-attempt
//! idempotency key, and retries are driven by the failure's retry
//! classification rather than error text. Filled in M7.

pub mod local_export;
pub mod overlord;

pub use local_export::LocalExportAdapter;
pub use overlord::{DestinationAdapter, LocalOverlordAdapter};
