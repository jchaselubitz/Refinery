//! The case-scoped local media store and provider upload lifecycle.
//!
//! Attachment bytes live under the data directory, one directory per case,
//! never in SQLite. [`store`] owns import — it materialises the bytes from
//! whichever source the request named, enforces the size cap, measures the
//! media type and digest instead of trusting the declaration, and writes the
//! result with owner-only permissions. [`sniff`] is the measurement.
//!
//! [`files_api`] is the Gemini Files API client: a resumable upload, then
//! polling to activation, because video is not readable the moment it lands.
//! [`lifecycle`] joins the two to storage and owns the states an attachment
//! moves through — `imported`, `uploading`, `available`, `expired`,
//! `rejected` — with every transition written as a durable event.
//!
//! The design turns on one decision: the provider's copy is a cache and the
//! local store holds the original. Gemini deletes uploaded files after about
//! two days, and a case that waits overnight for a human answer will routinely
//! resume against expired uploads. Because the bytes are still local, expiry
//! is a silent re-upload rather than a failed case.

pub mod files_api;
pub mod lifecycle;
pub mod sniff;
pub mod store;

pub use files_api::{ActivationPolicy, GeminiFilesClient, ProviderFile, ProviderFileState};
pub use lifecycle::{
    ensure_available, ensure_case_attachments_available, import_request_attachments, ImportSummary,
};
pub use sniff::{sniff, Sniffed};
pub use store::{hex_digest, ImportRejection, ImportedAttachment, MediaStore, RejectionCode};
