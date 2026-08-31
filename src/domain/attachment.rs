//! Attachments: what a source submits and what Refinery records about it.
//!
//! Two types cover the lifecycle. [`AttachmentInput`] is what arrives on the
//! wire — a declaration plus a way to get the bytes. [`AttachmentMetadata`] is
//! what Refinery stores after import: the verified facts plus the provider
//! upload state. They are deliberately separate, because a declared media type
//! is a claim and a recorded one is a measurement; the media store sniffs the
//! bytes and rejects a mismatch rather than trusting the submitter.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::ids::AttachmentId;
use crate::domain::limits;
use crate::domain::validation::{check_text, ValidationCode, ValidationReport};

/// The broad category of an attachment, which decides which backend
/// capabilities a case requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// Plain text carried alongside the transcript.
    Text,
    /// A still image.
    Image,
    /// A recording; requires native video input from the backend.
    Video,
    /// Any other file the destination may need to know about.
    File,
}

impl AttachmentKind {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            AttachmentKind::Text => "text",
            AttachmentKind::Image => "image",
            AttachmentKind::Video => "video",
            AttachmentKind::File => "file",
        }
    }
}

/// Where the bytes for an attachment come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttachmentSource {
    /// Text supplied directly in the request.
    InlineText {
        /// The text content.
        text: String,
    },
    /// Bytes supplied directly in the request, base64-encoded.
    InlineBase64 {
        /// The base64-encoded bytes.
        data: String,
    },
    /// A file on the same machine, read once at import time. Only sources on
    /// this machine can use this form, and the path is read exactly once into
    /// the case-owned media store rather than referenced later.
    LocalPath {
        /// Absolute path to the file.
        path: String,
    },
}

/// An attachment as submitted.
///
/// The byte source is flattened onto the attachment, so unknown-field
/// rejection is enforced by the `AttachmentSource` variants rather than by
/// `deny_unknown_fields` here, which Serde does not allow alongside `flatten`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentInput {
    /// A display name, typically the original file name.
    pub name: String,
    /// The category the source claims.
    pub kind: AttachmentKind,
    /// The IANA media type the source claims, verified at import.
    pub media_type: String,
    /// The size the source claims, verified at import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// The lowercase hex SHA-256 the source claims, verified at import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_sha256: Option<String>,
    /// Where to get the bytes.
    #[serde(flatten)]
    pub source: AttachmentSource,
}

impl AttachmentInput {
    /// Append this attachment's contract issues to `report` under `field`.
    pub(crate) fn validate_into(&self, report: &mut ValidationReport, field: &str) {
        check_text(
            report,
            &format!("{field}.name"),
            &self.name,
            limits::MAX_ATTACHMENT_NAME_CHARS,
            true,
        );
        check_text(
            report,
            &format!("{field}.media_type"),
            &self.media_type,
            limits::MAX_ATTACHMENT_NAME_CHARS,
            true,
        );
        if let Some(size) = self.size_bytes {
            if size > limits::MAX_ATTACHMENT_BYTES {
                report.add(
                    format!("{field}.size_bytes"),
                    ValidationCode::TooLong,
                    format!(
                        "must be at most {} bytes, declared {size}",
                        limits::MAX_ATTACHMENT_BYTES
                    ),
                );
            }
        }
        if let Some(digest) = &self.digest_sha256 {
            if !is_sha256_hex(digest) {
                report.add(
                    format!("{field}.digest_sha256"),
                    ValidationCode::Malformed,
                    "must be 64 lowercase hexadecimal characters",
                );
            }
        }
        if let AttachmentSource::LocalPath { path } = &self.source {
            if !std::path::Path::new(path).is_absolute() {
                report.add(
                    format!("{field}.path"),
                    ValidationCode::Malformed,
                    "must be an absolute path",
                );
            }
        }
    }
}

/// Whether a string is a lowercase hex SHA-256 digest.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Where an attachment stands between arriving and being usable by a backend.
///
/// `Expired` is not a failure. Provider file uploads expire server-side; the
/// bytes remain in the local media store, so an expired attachment is
/// re-uploaded on next use and the case continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentState {
    /// Bytes are in the case-owned media store and verified.
    Imported,
    /// An upload to the provider is in flight or awaiting activation.
    Uploading,
    /// The provider holds a live copy the backend can reference.
    Available,
    /// The provider copy expired; re-upload from the media store on next use.
    Expired,
    /// Import or upload refused the attachment; it will never be used.
    Rejected,
}

impl AttachmentState {
    /// Whether the backend may reference the provider copy right now.
    pub fn is_usable(self) -> bool {
        matches!(self, AttachmentState::Available)
    }

    /// Whether the local bytes must be uploaded before the backend can use it.
    pub fn needs_upload(self) -> bool {
        matches!(self, AttachmentState::Imported | AttachmentState::Expired)
    }
}

/// What Refinery knows about an attachment after import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentMetadata {
    /// Refinery's identifier for the attachment.
    pub id: AttachmentId,
    /// The display name carried from the source.
    pub name: String,
    /// The category, after verification.
    pub kind: AttachmentKind,
    /// The media type measured by sniffing the bytes.
    pub media_type: String,
    /// The measured size in bytes.
    pub size_bytes: u64,
    /// The measured lowercase hex SHA-256 of the bytes.
    pub digest_sha256: String,
    /// Where the attachment stands.
    pub state: AttachmentState,
    /// The provider's name for the uploaded copy, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_file_name: Option<String>,
    /// When the provider copy was uploaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploaded_at: Option<DateTime<Utc>>,
    /// When the provider copy expires, after which it is re-uploaded on use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Why the attachment was rejected, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_reason: Option<String>,
}

impl AttachmentMetadata {
    /// Whether the provider copy has expired as of `now`, independently of the
    /// recorded state. Storage may not have observed the expiry yet, so the
    /// upload path checks the clock rather than trusting the snapshot.
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        match self.state {
            AttachmentState::Expired => true,
            AttachmentState::Available => self.expires_at.is_some_and(|expiry| expiry <= now),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(source: AttachmentSource) -> AttachmentInput {
        AttachmentInput {
            name: "shot.png".into(),
            kind: AttachmentKind::Image,
            media_type: "image/png".into(),
            size_bytes: Some(1024),
            digest_sha256: None,
            source,
        }
    }

    #[test]
    fn an_oversized_declaration_is_rejected_before_any_bytes_are_read() {
        let mut attachment = input(AttachmentSource::InlineBase64 {
            data: "aGk=".into(),
        });
        attachment.size_bytes = Some(limits::MAX_ATTACHMENT_BYTES + 1);
        let mut report = ValidationReport::new();
        attachment.validate_into(&mut report, "attachments[0]");
        assert!(report.has_field("attachments[0].size_bytes"));
    }

    #[test]
    fn a_relative_local_path_is_rejected() {
        let attachment = input(AttachmentSource::LocalPath {
            path: "relative/shot.png".into(),
        });
        let mut report = ValidationReport::new();
        attachment.validate_into(&mut report, "attachments[0]");
        assert!(report.has_code(ValidationCode::Malformed));
    }

    #[test]
    fn a_malformed_digest_is_rejected() {
        let mut attachment = input(AttachmentSource::InlineText { text: "hi".into() });
        attachment.digest_sha256 = Some("NOTAHEXDIGEST".into());
        let mut report = ValidationReport::new();
        attachment.validate_into(&mut report, "attachments[0]");
        assert!(report.has_field("attachments[0].digest_sha256"));
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(!is_sha256_hex(&"A".repeat(64)));
    }

    #[test]
    fn the_source_is_flattened_onto_the_attachment() {
        let attachment = input(AttachmentSource::InlineText { text: "hi".into() });
        let json = serde_json::to_value(&attachment).unwrap();
        assert_eq!(json["source"], "inline_text");
        assert_eq!(json["text"], "hi");
    }

    #[test]
    fn expiry_is_read_from_the_clock_not_only_the_state() {
        let now = Utc::now();
        let metadata = AttachmentMetadata {
            id: AttachmentId::new(),
            name: "clip.mp4".into(),
            kind: AttachmentKind::Video,
            media_type: "video/mp4".into(),
            size_bytes: 10,
            digest_sha256: "a".repeat(64),
            state: AttachmentState::Available,
            provider_file_name: Some("files/abc".into()),
            uploaded_at: Some(now - chrono::Duration::hours(50)),
            expires_at: Some(now - chrono::Duration::hours(2)),
            rejected_reason: None,
        };
        assert!(metadata.is_expired_at(now));
        assert!(AttachmentState::Expired.needs_upload());
        assert!(!AttachmentState::Expired.is_usable());
    }
}
