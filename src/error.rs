//! The single application error type and its retry classification.
//!
//! Every fallible boundary in Refinery converts into [`AppError`]. The job
//! runner and the delivery machinery decide what to do next from
//! [`RetryClass`] alone; they never inspect error text. Module-level errors use
//! `thiserror` and convert inward, so the classification is decided where the
//! failure actually happened rather than guessed later.

use std::path::PathBuf;

/// How the caller should treat a failure.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    /// A transient failure. The operation may be retried with backoff.
    Retryable,
    /// A permanent failure. Retrying the same operation cannot succeed.
    NonRetryable,
    /// Progress requires a person: missing credentials, an unanswered
    /// question, or a policy decision only the user can make.
    NeedsUser,
}

impl RetryClass {
    /// Whether the job runner should schedule another attempt.
    pub fn is_retryable(self) -> bool {
        matches!(self, RetryClass::Retryable)
    }

    /// The stored and displayed spelling, matching the wire representation.
    pub fn as_str(self) -> &'static str {
        match self {
            RetryClass::Retryable => "retryable",
            RetryClass::NonRetryable => "non_retryable",
            RetryClass::NeedsUser => "needs_user",
        }
    }
}

impl std::fmt::Display for RetryClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The application error type.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Configuration is missing, unreadable, or invalid.
    #[error("configuration error: {message}")]
    Config {
        /// What is wrong with the configuration.
        message: String,
    },

    /// A filesystem operation failed on a path Refinery owns.
    #[error("filesystem error at {path}: {source}")]
    Io {
        /// The path the operation targeted.
        path: PathBuf,
        /// The underlying operating-system error.
        #[source]
        source: std::io::Error,
    },

    /// A request violated an input or output contract.
    #[error("invalid request: {message}")]
    Invalid {
        /// What was invalid about the request.
        message: String,
    },

    /// A requested entity does not exist.
    #[error("{kind} not found: {id}")]
    NotFound {
        /// The kind of entity, for example `case` or `repository`.
        kind: &'static str,
        /// The identifier that was looked up.
        id: String,
    },

    /// The same idempotency key arrived with different content.
    #[error("idempotency conflict for key {key}")]
    IdempotencyConflict {
        /// The conflicting idempotency key.
        key: String,
    },

    /// A repository, media, or API policy denied the operation.
    #[error("policy denied: {message}")]
    PolicyDenied {
        /// Why the policy denied the operation.
        message: String,
    },

    /// Durable storage failed.
    #[error("storage error: {message}")]
    Storage {
        /// What the storage layer was doing when it failed.
        message: String,
        /// Whether the storage failure is worth retrying (locking, busy) or
        /// permanent (constraint violation, corruption).
        retry: RetryClass,
    },

    /// A model provider or destination call failed.
    #[error("{service} error: {message}")]
    Upstream {
        /// The service that failed, for example `gemini` or `overlord`.
        service: &'static str,
        /// The redacted failure description.
        message: String,
        /// The classification decided at the call site.
        retry: RetryClass,
    },

    /// Progress requires the user: an unanswered question or absent credential.
    #[error("user action required: {message}")]
    NeedsUser {
        /// What the user must do.
        message: String,
    },

    /// A command or capability exists in the product but is not built yet in
    /// this milestone.
    #[error("{feature} is not available yet ({arrives_in})")]
    Unsupported {
        /// The capability the caller asked for.
        feature: String,
        /// Where the capability arrives, for example `milestone M3`.
        arrives_in: &'static str,
    },

    /// An unexpected internal failure.
    #[error("internal error: {0:#}")]
    Internal(#[source] anyhow::Error),
}

impl AppError {
    /// The retry classification for this error.
    pub fn retry_class(&self) -> RetryClass {
        match self {
            AppError::Config { .. }
            | AppError::Invalid { .. }
            | AppError::NotFound { .. }
            | AppError::IdempotencyConflict { .. }
            | AppError::PolicyDenied { .. }
            | AppError::Unsupported { .. }
            | AppError::Internal(_) => RetryClass::NonRetryable,
            AppError::Io { .. } => RetryClass::Retryable,
            AppError::NeedsUser { .. } => RetryClass::NeedsUser,
            AppError::Storage { retry, .. } | AppError::Upstream { retry, .. } => *retry,
        }
    }

    /// A stable, machine-readable code for API bodies, logs, and diagnostics.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Config { .. } => "config_error",
            AppError::Io { .. } => "io_error",
            AppError::Invalid { .. } => "invalid_request",
            AppError::NotFound { .. } => "not_found",
            AppError::IdempotencyConflict { .. } => "idempotency_conflict",
            AppError::PolicyDenied { .. } => "policy_denied",
            AppError::Storage { .. } => "storage_error",
            AppError::Upstream { .. } => "upstream_error",
            AppError::NeedsUser { .. } => "needs_user",
            AppError::Unsupported { .. } => "unsupported",
            AppError::Internal(_) => "internal_error",
        }
    }

    /// The human-facing description without the classification prefix.
    ///
    /// `Display` prefixes each variant so a bare error is self-describing on a
    /// terminal ("user action required: …"). Somewhere that already states the
    /// status — a `doctor` line marked `fail`, an API body carrying a `code` —
    /// that prefix is noise, and this is the text to use instead.
    pub fn detail(&self) -> String {
        match self {
            AppError::Config { message }
            | AppError::Invalid { message }
            | AppError::PolicyDenied { message }
            | AppError::Storage { message, .. }
            | AppError::NeedsUser { message } => message.clone(),
            AppError::Upstream {
                service, message, ..
            } => format!("{service}: {message}"),
            other => other.to_string(),
        }
    }

    /// Build a configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        AppError::Config {
            message: message.into(),
        }
    }

    /// Build a filesystem error for a path Refinery owns.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        AppError::Io {
            path: path.into(),
            source,
        }
    }

    /// Build an error for a capability that a later milestone delivers.
    pub fn unsupported(feature: impl Into<String>, arrives_in: &'static str) -> Self {
        AppError::Unsupported {
            feature: feature.into(),
            arrives_in,
        }
    }

    /// Build a contract-violation error.
    pub fn invalid(message: impl Into<String>) -> Self {
        AppError::Invalid {
            message: message.into(),
        }
    }
}

/// The application result alias.
pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_follows_the_variant() {
        assert_eq!(
            AppError::config("no key").retry_class(),
            RetryClass::NonRetryable
        );
        assert_eq!(
            AppError::io("/tmp/x", std::io::Error::other("disk")).retry_class(),
            RetryClass::Retryable
        );
        assert_eq!(
            AppError::NeedsUser {
                message: "answer the question".into()
            }
            .retry_class(),
            RetryClass::NeedsUser
        );
    }

    #[test]
    fn upstream_carries_the_call_site_classification() {
        let err = AppError::Upstream {
            service: "gemini",
            message: "429 rate limited".into(),
            retry: RetryClass::Retryable,
        };
        assert!(err.retry_class().is_retryable());
        assert_eq!(err.code(), "upstream_error");
    }

    #[test]
    fn codes_are_stable_strings() {
        assert_eq!(
            AppError::NotFound {
                kind: "case",
                id: "abc".into()
            }
            .code(),
            "not_found"
        );
    }
}
