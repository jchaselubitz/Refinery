//! Refinery turns transcripts, repository context, images, video, and
//! follow-up answers into validated prompts for destinations such as Overlord.
//!
//! One local binary owns one SQLite database and one data directory. The
//! module tree mirrors the product's internal boundaries: contracts in
//! [`domain`], orchestration in [`cases`], provider work behind [`agent`],
//! durable state in [`storage`] and [`jobs`], and every outward-facing surface
//! in [`api`] and [`integrations`].

#![warn(missing_docs)]

pub mod agent;
pub mod api;
pub mod app;
pub mod cases;
pub mod cli;
pub mod config;
#[cfg(feature = "desktop")]
pub mod desktop;
pub mod diagnostics;
pub mod domain;
pub mod error;
pub mod evaluations;
pub mod integrations;
pub mod interactions;
pub mod jobs;
pub mod media;
pub mod repositories;
pub mod storage;

pub use error::{AppError, Result, RetryClass};
