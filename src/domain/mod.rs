//! Wire and internal contracts: refinement requests, transcripts, attachment
//! metadata, questions and answers, refined prompts, case state, case events,
//! backend capabilities, and the Overlord delivery envelope.
//!
//! Every externally visible type here carries an explicit `schema_version` and
//! has a Schemars-generated JSON Schema committed under `schemas/`, so Overlord
//! and future clients validate integrations without importing Rust code. This
//! module is the contract freeze point: after milestone M1, changing a type
//! here in a way that changes its schema requires a version bump, and the
//! schema-drift check in CI is what makes that unavoidable rather than
//! aspirational.
//!
//! Three rules hold across every type in this module:
//!
//! - **Validation is pure.** Every `validate` is deterministic — no clock, no
//!   filesystem, no network — so contracts can be checked before anything is
//!   written and the same check can be replayed in a test or handed back to a
//!   model as a corrective retry.
//! - **Every issue is reported at once.** Validation returns a
//!   [`ValidationReport`], not the first problem it hit, because a person
//!   fixing a submission should see the whole list.
//! - **Submitted content is data.** Transcripts, attachments, and metadata are
//!   carried and bounded, never interpreted as instructions.

pub mod api;
pub mod attachment;
pub mod capabilities;
pub mod case;
pub mod delivery;
pub mod ids;
pub mod interaction;
pub mod limits;
pub mod prompt;
pub mod request;
pub mod secret;
pub mod transcript;
pub mod validation;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use api::ApiError;
pub use attachment::{
    AttachmentInput, AttachmentKind, AttachmentMetadata, AttachmentSource, AttachmentState,
};
pub use capabilities::{BackendCapabilities, Capability, RequiredCapabilities};
pub use case::{CaseEvent, CaseEventPayload, CaseState, CaseTransition, Outcome, UsageMetadata};
pub use delivery::{DeliveredAttachment, DeliveryAttempt, DeliveryEnvelope, DeliveryReceipt};
pub use ids::{
    AttachmentId, CaseId, DeliveryId, EventId, InvalidId, JobId, OutputId, QuestionId,
    QuestionRequestId, RepositoryId, RunId,
};
pub use interaction::{
    Answer, AnswerSet, AnswerValue, Choice, Question, QuestionRequest, QuestionRequestStatus,
    ResponseType,
};
pub use prompt::{PromptReference, PromptValidationContext, RefinedPrompt};
pub use request::{
    Destination, LocalExportDestination, OverlordDestination, RefinementRequest, SourceCallback,
    SourceRef, SourceSystem,
};
pub use secret::SecretString;
pub use transcript::{MessageRole, Transcript, TranscriptMessage};
pub use validation::{ValidationCode, ValidationIssue, ValidationReport};

/// The contract version this build speaks.
///
/// One number covers every contract in this module rather than one per type.
/// The types are submitted, answered, and delivered together, so a client that
/// understands one version understands the set; per-type versions would let a
/// caller assemble a combination nobody ever tested.
pub const CONTRACT_VERSION: u32 = 1;

/// The version stamped on and required of every wire contract.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    /// The version this build produces.
    pub const CURRENT: SchemaVersion = SchemaVersion(CONTRACT_VERSION);

    /// Name a specific version, for tests and for reading older records.
    pub const fn new(version: u32) -> Self {
        Self(version)
    }

    /// The version as a number.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Whether this build understands the version.
    ///
    /// Strict equality, deliberately. A newer contract may have added a
    /// required field this build would silently ignore, and an older one may
    /// have meant something different by the same field; refusing is honest,
    /// and migration is a decision to make explicitly when a second version
    /// exists.
    pub const fn is_supported(self) -> bool {
        self.0 == CONTRACT_VERSION
    }
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self::CURRENT
    }
}

impl std::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_version_is_supported_and_nothing_else_is() {
        assert!(SchemaVersion::CURRENT.is_supported());
        assert!(!SchemaVersion::new(CONTRACT_VERSION + 1).is_supported());
        assert!(!SchemaVersion::new(0).is_supported());
    }

    #[test]
    fn a_schema_version_is_a_bare_number_on_the_wire() {
        assert_eq!(serde_json::to_string(&SchemaVersion::CURRENT).unwrap(), "1");
        let parsed: SchemaVersion = serde_json::from_str("1").unwrap();
        assert_eq!(parsed, SchemaVersion::CURRENT);
    }

    #[test]
    fn the_default_is_the_current_version() {
        assert_eq!(SchemaVersion::default(), SchemaVersion::CURRENT);
    }
}
