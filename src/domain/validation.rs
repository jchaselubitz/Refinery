//! Deterministic contract validation results.
//!
//! Contract validation never returns a single opaque message. It returns every
//! issue it found, each naming the field, a stable machine-readable code, and
//! a human sentence. That shape serves three consumers at once: the API
//! returns it in the uniform error body, the local interface renders it beside
//! the offending field, and the agent loop feeds it back to the model as the
//! one corrective retry for a malformed `submit_refined_prompt`.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One thing wrong with a contract value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ValidationIssue {
    /// Dotted path to the offending field, for example
    /// `acceptance_criteria[0]`. Empty when the issue is about the value as a
    /// whole.
    pub field: String,
    /// A stable code callers may branch on, for example `empty` or `too_long`.
    pub code: ValidationCode,
    /// A sentence a person can act on.
    pub message: String,
}

impl ValidationIssue {
    /// Build an issue for a field.
    pub fn new(field: impl Into<String>, code: ValidationCode, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.field, self.message)
        }
    }
}

/// The stable reason a value was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    /// A required value was absent or blank.
    Empty,
    /// A value exceeded its contract limit.
    TooLong,
    /// A collection had more items than its contract limit.
    TooMany,
    /// A collection had fewer items than the contract requires.
    TooFew,
    /// A value was outside the set the contract allows.
    NotAllowed,
    /// A value was syntactically malformed.
    Malformed,
    /// A value repeated where the contract requires uniqueness.
    Duplicate,
    /// A referenced entity does not exist.
    Unknown,
    /// A required question has not been answered.
    Unanswered,
    /// The schema version is not one this build understands.
    UnsupportedSchemaVersion,
    /// The value carried provider or transport markup that must never reach a
    /// destination.
    ProviderMarkup,
}

/// The outcome of validating one contract value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ValidationReport {
    /// Every issue found, in field order. Empty means valid.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    /// An empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an issue.
    pub fn push(&mut self, issue: ValidationIssue) {
        self.issues.push(issue);
    }

    /// Record an issue built from its parts.
    pub fn add(
        &mut self,
        field: impl Into<String>,
        code: ValidationCode,
        message: impl Into<String>,
    ) {
        self.push(ValidationIssue::new(field, code, message));
    }

    /// Whether the value passed every check.
    pub fn is_valid(&self) -> bool {
        self.issues.is_empty()
    }

    /// Convert into a result so callers can use `?`.
    pub fn into_result(self) -> Result<(), Self> {
        if self.is_valid() {
            Ok(())
        } else {
            Err(self)
        }
    }

    /// Whether any issue carries the given code.
    pub fn has_code(&self, code: ValidationCode) -> bool {
        self.issues.iter().any(|issue| issue.code == code)
    }

    /// Whether any issue names the given field.
    pub fn has_field(&self, field: &str) -> bool {
        self.issues.iter().any(|issue| issue.field == field)
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered: Vec<String> = self.issues.iter().map(ToString::to_string).collect();
        f.write_str(&rendered.join("; "))
    }
}

/// Check that a string is non-empty after trimming and within its limit.
pub(crate) fn check_text(
    report: &mut ValidationReport,
    field: &str,
    value: &str,
    max_chars: usize,
    required: bool,
) {
    if value.trim().is_empty() {
        if required {
            report.add(field, ValidationCode::Empty, "must not be empty");
        }
        return;
    }
    let length = value.chars().count();
    if length > max_chars {
        report.add(
            field,
            ValidationCode::TooLong,
            format!("must be at most {max_chars} characters, found {length}"),
        );
    }
}

/// Check a list of plain strings for count and per-item limits.
pub(crate) fn check_string_list(
    report: &mut ValidationReport,
    field: &str,
    values: &[String],
    max_items: usize,
    max_chars: usize,
) {
    if values.len() > max_items {
        report.add(
            field,
            ValidationCode::TooMany,
            format!(
                "must have at most {max_items} entries, found {}",
                values.len()
            ),
        );
    }
    for (index, value) in values.iter().enumerate() {
        check_text(report, &format!("{field}[{index}]"), value, max_chars, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_no_issues_is_valid() {
        let report = ValidationReport::new();
        assert!(report.is_valid());
        assert!(report.into_result().is_ok());
    }

    #[test]
    fn text_checks_flag_blank_and_oversized_values() {
        let mut report = ValidationReport::new();
        check_text(&mut report, "title", "   ", 10, true);
        check_text(&mut report, "prompt", "abcdefghijk", 10, true);
        assert!(report.has_code(ValidationCode::Empty));
        assert!(report.has_code(ValidationCode::TooLong));
        assert!(report.has_field("prompt"));
    }

    #[test]
    fn an_optional_blank_value_is_accepted() {
        let mut report = ValidationReport::new();
        check_text(&mut report, "note", "", 10, false);
        assert!(report.is_valid());
    }

    #[test]
    fn list_checks_report_the_offending_index() {
        let mut report = ValidationReport::new();
        check_string_list(&mut report, "criteria", &["ok".into(), "".into()], 5, 10);
        assert!(report.has_field("criteria[1]"));
    }
}
