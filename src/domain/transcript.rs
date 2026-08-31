//! The transcript: the conversation a source hands Refinery to refine.
//!
//! Transcript content is untrusted model input. Roles are normalized to
//! Refinery's own small set rather than mirroring any provider's vocabulary,
//! because a role in a submitted transcript describes who said something in
//! the *source* conversation and must never be mistaken for authority in
//! Refinery's own provider conversation.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::limits;
use crate::domain::validation::{check_text, ValidationCode, ValidationReport};

/// Who produced a transcript message in the source conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// A person.
    User,
    /// An assistant or agent in the source conversation.
    Assistant,
    /// System or instruction text from the source system.
    System,
    /// Tool or command output quoted into the conversation.
    Tool,
}

/// One message in a submitted transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptMessage {
    /// The source system's identifier for this message, when it has one.
    /// Carried so a destination can correlate back, never interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Who produced the message.
    pub role: MessageRole,
    /// The message text.
    pub content: String,
    /// A display name for the author, when the source supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// When the message was produced, in UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

/// An ordered conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    /// Messages in the order they occurred, oldest first.
    pub messages: Vec<TranscriptMessage>,
}

impl Transcript {
    /// Build a transcript from its messages.
    pub fn new(messages: Vec<TranscriptMessage>) -> Self {
        Self { messages }
    }

    /// Whether the transcript carries no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Total characters across every message.
    pub fn total_chars(&self) -> usize {
        self.messages
            .iter()
            .map(|message| message.content.chars().count())
            .sum()
    }

    /// Append this transcript's contract issues to `report` under `field`.
    pub(crate) fn validate_into(&self, report: &mut ValidationReport, field: &str) {
        if self.messages.is_empty() {
            report.add(
                field,
                ValidationCode::Empty,
                "a refinement needs at least one transcript message",
            );
        }
        if self.messages.len() > limits::MAX_TRANSCRIPT_MESSAGES {
            report.add(
                field,
                ValidationCode::TooMany,
                format!(
                    "must have at most {} messages, found {}",
                    limits::MAX_TRANSCRIPT_MESSAGES,
                    self.messages.len()
                ),
            );
        }
        for (index, message) in self.messages.iter().enumerate() {
            check_text(
                report,
                &format!("{field}.messages[{index}].content"),
                &message.content,
                limits::MAX_MESSAGE_CHARS,
                true,
            );
        }
        let total = self.total_chars();
        if total > limits::MAX_TRANSCRIPT_CHARS {
            report.add(
                field,
                ValidationCode::TooLong,
                format!(
                    "must be at most {} characters in total, found {total}",
                    limits::MAX_TRANSCRIPT_CHARS
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(content: &str) -> TranscriptMessage {
        TranscriptMessage {
            id: None,
            role: MessageRole::User,
            content: content.to_owned(),
            author: None,
            created_at: None,
        }
    }

    #[test]
    fn an_empty_transcript_is_rejected() {
        let mut report = ValidationReport::new();
        Transcript::default().validate_into(&mut report, "transcript");
        assert!(report.has_code(ValidationCode::Empty));
    }

    #[test]
    fn a_blank_message_is_rejected_by_index() {
        let mut report = ValidationReport::new();
        Transcript::new(vec![message("fine"), message("  ")])
            .validate_into(&mut report, "transcript");
        assert!(report.has_field("transcript.messages[1].content"));
    }

    #[test]
    fn the_whole_transcript_is_bounded_not_just_each_message() {
        let big = "x".repeat(limits::MAX_MESSAGE_CHARS);
        let messages = (0..6).map(|_| message(&big)).collect();
        let mut report = ValidationReport::new();
        Transcript::new(messages).validate_into(&mut report, "transcript");
        assert!(report.has_code(ValidationCode::TooLong));
    }

    #[test]
    fn unknown_fields_are_refused() {
        let json = r#"{"role":"user","content":"hi","extra":true}"#;
        assert!(serde_json::from_str::<TranscriptMessage>(json).is_err());
    }
}
