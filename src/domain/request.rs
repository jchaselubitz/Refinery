//! The input contract: what a source submits to start a case.
//!
//! `RefinementRequest` is the widest contract Refinery exposes, so it carries
//! the full versioning and validation discipline: an explicit
//! `schema_version`, unknown-field rejection, and a deterministic
//! [`RefinementRequest::validate`] that runs before any storage write or
//! provider call. Everything reachable from here is untrusted: transcripts,
//! attachment declarations, and metadata are data, never instructions.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::attachment::AttachmentInput;
use crate::domain::ids::RepositoryId;
use crate::domain::limits;
use crate::domain::secret::SecretString;
use crate::domain::transcript::Transcript;
use crate::domain::validation::{check_text, ValidationCode, ValidationReport};
use crate::domain::SchemaVersion;

/// The system that submitted a case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceSystem {
    /// An Overlord instance on this machine.
    Overlord,
    /// Refinery's own local browser interface.
    LocalUi,
    /// Refinery's command line.
    Cli,
    /// Any other local caller, named by the caller.
    Other(String),
}

/// Where Refinery sends questions for a case that came from a source with its
/// own interface.
///
/// The local interface needs no callback: it reads the case's event stream.
/// A source such as Overlord declares one at submission so a question reaches
/// the person where they already are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceCallback {
    /// The loopback URL that receives question notifications.
    pub questions_url: String,
    /// The bearer token to present, when the source requires one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<SecretString>,
}

/// Who submitted a case and how to reach them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    /// The submitting system.
    pub system: SourceSystem,
    /// Which instance of that system, so two Overlord installations on one
    /// machine stay distinguishable.
    pub instance: String,
    /// Where to deliver questions, when the source handles them itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<SourceCallback>,
}

/// Where a completed refinement should be sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Destination {
    /// Submit the refined prompt to a local Overlord instance.
    Overlord(OverlordDestination),
    /// Write the refined prompt to a local file.
    LocalExport(LocalExportDestination),
}

impl Destination {
    /// The wire spelling of the destination kind, for events and logs.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Destination::Overlord(_) => "overlord",
            Destination::LocalExport(_) => "local_export",
        }
    }
}

/// An Overlord submission target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OverlordDestination {
    /// The loopback base URL of the Overlord instance.
    pub base_url: String,
    /// The mission the refined prompt belongs to, when known at submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// The objective the refined prompt belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective_id: Option<String>,
    /// The bearer token for the Overlord endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<SecretString>,
}

/// A local file export target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalExportDestination {
    /// Absolute path of the file to write.
    pub path: String,
}

/// A versioned refinement submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefinementRequest {
    /// The contract version this request is written against.
    pub schema_version: SchemaVersion,
    /// The source's stable identifier for the thing being refined.
    pub request_id: String,
    /// The retry-safe submission key. The same key with equivalent content
    /// returns the existing case; the same key with different content is a
    /// conflict.
    pub idempotency_key: String,
    /// Who submitted, and where questions go.
    pub source: SourceRef,
    /// The conversation to refine.
    pub transcript: Transcript,
    /// A short description of the intended task, when the source has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_hint: Option<String>,
    /// Attachments to import into the case-owned media store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentInput>,
    /// The registered repository the agent may read, when the case is grounded
    /// in code. Repositories are registered out of band; a request names one,
    /// it never introduces one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryId>,
    /// Where the completed result should be sent.
    pub destination: Destination,
    /// Bounded source-specific correlation data, carried through to delivery
    /// and never interpreted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl RefinementRequest {
    /// Check the request against the input contract, returning every issue.
    ///
    /// This is deterministic and pure: no filesystem, no network, no clock. It
    /// runs before ingress writes anything, so a malformed submission never
    /// creates a case.
    pub fn validate(&self) -> Result<(), ValidationReport> {
        let mut report = ValidationReport::new();

        if !self.schema_version.is_supported() {
            report.add(
                "schema_version",
                ValidationCode::UnsupportedSchemaVersion,
                format!(
                    "this build understands schema version {}, received {}",
                    SchemaVersion::CURRENT,
                    self.schema_version
                ),
            );
        }

        check_text(
            &mut report,
            "request_id",
            &self.request_id,
            limits::MAX_IDENTIFIER_CHARS,
            true,
        );
        check_text(
            &mut report,
            "idempotency_key",
            &self.idempotency_key,
            limits::MAX_IDENTIFIER_CHARS,
            true,
        );
        check_text(
            &mut report,
            "source.instance",
            &self.source.instance,
            limits::MAX_IDENTIFIER_CHARS,
            true,
        );
        if let SourceSystem::Other(name) = &self.source.system {
            check_text(
                &mut report,
                "source.system",
                name,
                limits::MAX_IDENTIFIER_CHARS,
                true,
            );
        }
        if let Some(callback) = &self.source.callback {
            check_loopback_url(
                &mut report,
                "source.callback.questions_url",
                &callback.questions_url,
            );
        }

        self.transcript.validate_into(&mut report, "transcript");

        if let Some(hint) = &self.task_hint {
            check_text(
                &mut report,
                "task_hint",
                hint,
                limits::MAX_TASK_HINT_CHARS,
                true,
            );
        }

        if self.attachments.len() > limits::MAX_ATTACHMENTS {
            report.add(
                "attachments",
                ValidationCode::TooMany,
                format!(
                    "must have at most {} attachments, found {}",
                    limits::MAX_ATTACHMENTS,
                    self.attachments.len()
                ),
            );
        }
        for (index, attachment) in self.attachments.iter().enumerate() {
            attachment.validate_into(&mut report, &format!("attachments[{index}]"));
        }

        match &self.destination {
            Destination::Overlord(overlord) => {
                check_loopback_url(&mut report, "destination.base_url", &overlord.base_url);
            }
            Destination::LocalExport(export) => {
                if !std::path::Path::new(&export.path).is_absolute() {
                    report.add(
                        "destination.path",
                        ValidationCode::Malformed,
                        "must be an absolute path",
                    );
                }
            }
        }

        if self.metadata.len() > limits::MAX_METADATA_ENTRIES {
            report.add(
                "metadata",
                ValidationCode::TooMany,
                format!(
                    "must have at most {} entries, found {}",
                    limits::MAX_METADATA_ENTRIES,
                    self.metadata.len()
                ),
            );
        }
        for (key, value) in &self.metadata {
            check_text(
                &mut report,
                &format!("metadata.{key}"),
                key,
                limits::MAX_METADATA_KEY_CHARS,
                true,
            );
            check_text(
                &mut report,
                &format!("metadata.{key}"),
                value,
                limits::MAX_METADATA_VALUE_CHARS,
                false,
            );
        }

        report.into_result()
    }

    /// Whether the case needs a backend that accepts images.
    pub fn needs_image_input(&self) -> bool {
        self.attachments
            .iter()
            .any(|a| a.kind == crate::domain::attachment::AttachmentKind::Image)
    }

    /// Whether the case needs a backend that accepts video natively.
    pub fn needs_video_input(&self) -> bool {
        self.attachments
            .iter()
            .any(|a| a.kind == crate::domain::attachment::AttachmentKind::Video)
    }
}

/// Reject any URL that is not plain loopback HTTP.
///
/// Refinery never opens an outbound connection to an arbitrary host on a
/// submitter's say-so; both the question callback and the Overlord destination
/// are local by product design, so a non-loopback URL is a contract error
/// rather than a runtime failure.
fn check_loopback_url(report: &mut ValidationReport, field: &str, value: &str) {
    let Some(rest) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    else {
        report.add(
            field,
            ValidationCode::Malformed,
            "must be an http or https URL",
        );
        return;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        report.add(
            field,
            ValidationCode::NotAllowed,
            format!("must address loopback, found host {host}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attachment::{AttachmentKind, AttachmentSource};
    use crate::domain::transcript::{MessageRole, TranscriptMessage};

    fn request() -> RefinementRequest {
        RefinementRequest {
            schema_version: SchemaVersion::CURRENT,
            request_id: "ovld:coo-885".into(),
            idempotency_key: "key-1".into(),
            source: SourceRef {
                system: SourceSystem::Overlord,
                instance: "local".into(),
                callback: Some(SourceCallback {
                    questions_url: "http://127.0.0.1:7788/refinery/questions".into(),
                    bearer_token: Some(SecretString::new("t0ken")),
                }),
            },
            transcript: Transcript::new(vec![TranscriptMessage {
                id: None,
                role: MessageRole::User,
                content: "make the importer resumable".into(),
                author: None,
                created_at: None,
            }]),
            task_hint: Some("plan the work".into()),
            attachments: vec![],
            repository: None,
            destination: Destination::Overlord(OverlordDestination {
                base_url: "http://localhost:7788".into(),
                mission_id: Some("coo:885".into()),
                objective_id: None,
                bearer_token: None,
            }),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn a_well_formed_request_validates() {
        request().validate().unwrap();
    }

    #[test]
    fn an_unsupported_schema_version_is_reported_not_ignored() {
        let mut request = request();
        request.schema_version = SchemaVersion::new(99);
        let report = request.validate().unwrap_err();
        assert!(report.has_code(ValidationCode::UnsupportedSchemaVersion));
    }

    #[test]
    fn a_non_loopback_callback_is_refused() {
        let mut request = request();
        request.source.callback = Some(SourceCallback {
            questions_url: "https://example.com/hook".into(),
            bearer_token: None,
        });
        let report = request.validate().unwrap_err();
        assert!(report.has_field("source.callback.questions_url"));
        assert!(report.has_code(ValidationCode::NotAllowed));
    }

    #[test]
    fn every_issue_is_reported_at_once() {
        let mut request = request();
        request.request_id = String::new();
        request.idempotency_key = "  ".into();
        request.transcript = Transcript::default();
        let report = request.validate().unwrap_err();
        assert_eq!(report.issues.len(), 3, "{report}");
    }

    #[test]
    fn capability_needs_follow_from_the_attachments() {
        let mut request = request();
        assert!(!request.needs_image_input());
        assert!(!request.needs_video_input());
        request.attachments.push(AttachmentInput {
            name: "clip.mp4".into(),
            kind: AttachmentKind::Video,
            media_type: "video/mp4".into(),
            size_bytes: Some(10),
            digest_sha256: None,
            source: AttachmentSource::LocalPath {
                path: "/tmp/clip.mp4".into(),
            },
        });
        assert!(request.needs_video_input());
        request.validate().unwrap();
    }

    #[test]
    fn a_secret_in_the_request_never_reaches_a_debug_line() {
        let rendered = format!("{:?}", request());
        assert!(!rendered.contains("t0ken"), "{rendered}");
    }

    #[test]
    fn unknown_fields_are_refused_at_the_contract_boundary() {
        let mut value = serde_json::to_value(request()).unwrap();
        value["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<RefinementRequest>(value).is_err());
    }
}
