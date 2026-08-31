//! Diagnostics: logging, health checks, and the operator-facing status view.
//!
//! `refinery doctor` and `refinery status` are built here in M3; this module
//! currently owns logging setup only.

pub mod doctor;
pub mod fixtures;
pub mod logging;
pub mod redaction;
pub mod service;
