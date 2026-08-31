//! The uniform error body every local API route returns.
//!
//! One shape for every failure, so the browser interface, the CLI, and
//! Overlord all parse errors the same way. The `code` is the stable
//! machine-readable string from [`AppError::code`]; `retry_class` tells a
//! caller whether repeating the request could ever help; `issues` carries the
//! per-field detail from contract validation so a client can point at the
//! offending field instead of showing a paragraph.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::validation::{ValidationIssue, ValidationReport};
use crate::domain::SchemaVersion;
use crate::error::{AppError, RetryClass};

/// The body of any failed API response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    /// The contract version.
    pub schema_version: SchemaVersion,
    /// The stable machine-readable code, for example `invalid_request`.
    pub code: String,
    /// A person-readable, secret-free sentence.
    pub message: String,
    /// Whether repeating the request could help.
    pub retry_class: RetryClass,
    /// Per-field detail, when the failure was contract validation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<ValidationIssue>,
}

impl ApiError {
    /// Build an error body from an application error.
    ///
    /// The message comes from `Display`, which every variant keeps free of
    /// credentials; secrets are redacted by their own types before they could
    /// reach here.
    pub fn from_app_error(error: &AppError) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            code: error.code().to_owned(),
            message: error.to_string(),
            retry_class: error.retry_class(),
            issues: Vec::new(),
        }
    }

    /// Build an error body from a failed contract validation.
    pub fn from_validation(report: ValidationReport) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            code: "invalid_request".to_owned(),
            message: "the request did not satisfy the contract".to_owned(),
            retry_class: RetryClass::NonRetryable,
            issues: report.issues,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::validation::ValidationCode;

    #[test]
    fn an_application_error_becomes_a_coded_body() {
        let body = ApiError::from_app_error(&AppError::NotFound {
            kind: "case",
            id: "abc".into(),
        });
        assert_eq!(body.code, "not_found");
        assert_eq!(body.retry_class, RetryClass::NonRetryable);
        assert!(body.issues.is_empty());
    }

    #[test]
    fn a_validation_failure_carries_the_offending_fields() {
        let mut report = ValidationReport::new();
        report.add("transcript", ValidationCode::Empty, "must not be empty");
        let body = ApiError::from_validation(report);
        assert_eq!(body.code, "invalid_request");
        assert_eq!(body.issues[0].field, "transcript");
    }

    #[test]
    fn a_retryable_upstream_failure_says_so() {
        let body = ApiError::from_app_error(&AppError::Upstream {
            service: "gemini",
            message: "503".into(),
            retry: RetryClass::Retryable,
        });
        assert_eq!(body.retry_class, RetryClass::Retryable);
    }
}
