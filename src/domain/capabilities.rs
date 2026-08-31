//! What an agent backend can do, and what a case needs.
//!
//! The product principle is explicit capability handling: Refinery selects or
//! rejects a backend *before* starting a case, rather than discovering
//! mid-refinement that a provider cannot see the video the user attached. That
//! only works if "what this case needs" is derivable from the request, so both
//! halves of the comparison are types here and the check is a pure function.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::request::RefinementRequest;
use crate::domain::SchemaVersion;

/// One thing a backend may or may not be able to do.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Accepts text prompts.
    TextInput,
    /// Accepts images as input.
    ImageInput,
    /// Accepts video as input without a transcoding or framing workaround.
    NativeVideoInput,
    /// Can call declared functions.
    ToolCalling,
    /// Can be constrained to a response schema.
    StructuredOutput,
    /// A conversation can be resumed after an interruption, whether by a
    /// provider session or, as with Gemini, by replaying persisted history.
    ResumableConversation,
    /// Brings its own repository runtime, as a hosted agent runtime does.
    RepositoryRuntime,
}

impl Capability {
    /// Every capability, for exhaustive checks.
    pub const ALL: &'static [Capability] = &[
        Capability::TextInput,
        Capability::ImageInput,
        Capability::NativeVideoInput,
        Capability::ToolCalling,
        Capability::StructuredOutput,
        Capability::ResumableConversation,
        Capability::RepositoryRuntime,
    ];

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::TextInput => "text_input",
            Capability::ImageInput => "image_input",
            Capability::NativeVideoInput => "native_video_input",
            Capability::ToolCalling => "tool_calling",
            Capability::StructuredOutput => "structured_output",
            Capability::ResumableConversation => "resumable_conversation",
            Capability::RepositoryRuntime => "repository_runtime",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one backend can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackendCapabilities {
    /// The contract version.
    pub schema_version: SchemaVersion,
    /// The backend this describes, for example `gemini`.
    pub backend_id: String,
    /// The model or runtime version, for the audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Accepts text prompts.
    pub text_input: bool,
    /// Accepts images.
    pub image_input: bool,
    /// Accepts video natively.
    pub native_video_input: bool,
    /// Can call declared functions.
    pub tool_calling: bool,
    /// Can be constrained to a response schema.
    pub structured_output: bool,
    /// Conversations survive an interruption.
    pub resumable_conversation: bool,
    /// Brings its own repository runtime.
    pub repository_runtime: bool,
}

impl BackendCapabilities {
    /// Whether the backend has a given capability.
    pub fn has(&self, capability: Capability) -> bool {
        match capability {
            Capability::TextInput => self.text_input,
            Capability::ImageInput => self.image_input,
            Capability::NativeVideoInput => self.native_video_input,
            Capability::ToolCalling => self.tool_calling,
            Capability::StructuredOutput => self.structured_output,
            Capability::ResumableConversation => self.resumable_conversation,
            Capability::RepositoryRuntime => self.repository_runtime,
        }
    }

    /// The required capabilities this backend lacks, in a stable order.
    pub fn missing_for(&self, required: &RequiredCapabilities) -> Vec<Capability> {
        required
            .capabilities
            .iter()
            .copied()
            .filter(|capability| !self.has(*capability))
            .collect()
    }

    /// Whether this backend can run a case with these requirements.
    pub fn satisfies(&self, required: &RequiredCapabilities) -> bool {
        self.missing_for(required).is_empty()
    }
}

/// What a particular case demands of a backend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequiredCapabilities {
    /// The capabilities the case needs, sorted and deduplicated so two equal
    /// requirement sets compare equal.
    pub capabilities: Vec<Capability>,
}

impl RequiredCapabilities {
    /// Build a requirement set, normalizing order and duplicates.
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        let mut capabilities: Vec<Capability> = capabilities.into_iter().collect();
        capabilities.sort_unstable();
        capabilities.dedup();
        Self { capabilities }
    }

    /// Derive what a submitted request needs.
    ///
    /// Every case needs text, tools, structured output, and resumability:
    /// refinement is a tool loop that can pause on a question and survive a
    /// restart, so those are not optional. Images, video, and a repository are
    /// demanded only when the request actually carries them.
    pub fn for_request(request: &RefinementRequest) -> Self {
        let mut capabilities = vec![
            Capability::TextInput,
            Capability::ToolCalling,
            Capability::StructuredOutput,
            Capability::ResumableConversation,
        ];
        if request.needs_image_input() {
            capabilities.push(Capability::ImageInput);
        }
        if request.needs_video_input() {
            capabilities.push(Capability::NativeVideoInput);
        }
        Self::new(capabilities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attachment::{AttachmentInput, AttachmentKind, AttachmentSource};
    use crate::domain::request::{Destination, LocalExportDestination, SourceRef, SourceSystem};
    use crate::domain::transcript::{MessageRole, Transcript, TranscriptMessage};

    fn gemini() -> BackendCapabilities {
        BackendCapabilities {
            schema_version: SchemaVersion::CURRENT,
            backend_id: "gemini".into(),
            model: Some("gemini-3.7-flash".into()),
            text_input: true,
            image_input: true,
            native_video_input: true,
            tool_calling: true,
            structured_output: true,
            resumable_conversation: true,
            repository_runtime: false,
        }
    }

    fn request(attachments: Vec<AttachmentInput>) -> RefinementRequest {
        RefinementRequest {
            schema_version: SchemaVersion::CURRENT,
            request_id: "r-1".into(),
            idempotency_key: "k-1".into(),
            source: SourceRef {
                system: SourceSystem::Cli,
                instance: "local".into(),
                callback: None,
            },
            transcript: Transcript::new(vec![TranscriptMessage {
                id: None,
                role: MessageRole::User,
                content: "refine this".into(),
                author: None,
                created_at: None,
            }]),
            task_hint: None,
            attachments,
            repository: None,
            destination: Destination::LocalExport(LocalExportDestination {
                path: "/tmp/out.json".into(),
            }),
            metadata: Default::default(),
        }
    }

    fn attachment(kind: AttachmentKind) -> AttachmentInput {
        AttachmentInput {
            name: "a".into(),
            kind,
            media_type: "application/octet-stream".into(),
            size_bytes: None,
            digest_sha256: None,
            source: AttachmentSource::InlineBase64 {
                data: "aGk=".into(),
            },
        }
    }

    #[test]
    fn every_case_needs_tools_structure_and_resumability() {
        let required = RequiredCapabilities::for_request(&request(vec![]));
        assert!(required.capabilities.contains(&Capability::ToolCalling));
        assert!(required
            .capabilities
            .contains(&Capability::StructuredOutput));
        assert!(required
            .capabilities
            .contains(&Capability::ResumableConversation));
        assert!(!required.capabilities.contains(&Capability::ImageInput));
    }

    #[test]
    fn media_attachments_add_their_capability() {
        let required =
            RequiredCapabilities::for_request(&request(vec![attachment(AttachmentKind::Video)]));
        assert!(required
            .capabilities
            .contains(&Capability::NativeVideoInput));
        assert!(gemini().satisfies(&required));
    }

    #[test]
    fn a_backend_missing_a_capability_is_rejected_with_the_reason() {
        let mut backend = gemini();
        backend.native_video_input = false;
        let required =
            RequiredCapabilities::for_request(&request(vec![attachment(AttachmentKind::Video)]));
        assert!(!backend.satisfies(&required));
        assert_eq!(
            backend.missing_for(&required),
            vec![Capability::NativeVideoInput]
        );
    }

    #[test]
    fn requirement_sets_normalize_so_equal_needs_compare_equal() {
        let a = RequiredCapabilities::new([Capability::ToolCalling, Capability::TextInput]);
        let b = RequiredCapabilities::new([
            Capability::TextInput,
            Capability::ToolCalling,
            Capability::TextInput,
        ]);
        assert_eq!(a, b);
    }

    #[test]
    fn the_capability_lookup_covers_every_capability() {
        let backend = gemini();
        for capability in Capability::ALL {
            let _ = backend.has(*capability);
        }
    }
}
