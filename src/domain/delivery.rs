//! The delivery envelope: what Refinery hands a destination, and what it
//! records about each attempt.
//!
//! The envelope is the only thing a destination ever sees. It carries the
//! refined prompt plus the correlation a destination needs to attach it to the
//! right work, and nothing about Refinery's internal conversation. That is the
//! product promise made structural: there is no field here through which
//! transcript, tool output, or provider framing could reach Overlord.
//!
//! Each attempt sets an idempotency key derived from the delivery and the
//! attempt number, so a retry after an ambiguous failure cannot create a second
//! piece of externally visible work.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::ids::{AttachmentId, CaseId, DeliveryId, OutputId, RepositoryId};
use crate::domain::prompt::RefinedPrompt;
use crate::domain::SchemaVersion;
use crate::error::RetryClass;
use std::collections::BTreeMap;

/// An attachment as a destination sees it: named and described, never inlined.
/// The bytes stay in Refinery's case-owned media store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveredAttachment {
    /// Refinery's identifier for the attachment.
    pub id: AttachmentId,
    /// The display name.
    pub name: String,
    /// The measured media type.
    pub media_type: String,
    /// The measured size.
    pub size_bytes: u64,
    /// The measured digest, so a destination can verify a copy it fetches.
    pub digest_sha256: String,
}

/// What Refinery sends a destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryEnvelope {
    /// The contract version.
    pub schema_version: SchemaVersion,
    /// The delivery this envelope belongs to.
    pub delivery_id: DeliveryId,
    /// The case that produced it.
    pub case_id: CaseId,
    /// The accepted output.
    pub output_id: OutputId,
    /// The source's original request identifier, so the destination can
    /// correlate the result with what it submitted.
    pub request_id: String,
    /// The key the destination must treat as the deduplication identity for
    /// this attempt.
    pub idempotency_key: String,
    /// When the envelope was produced, in UTC.
    pub produced_at: DateTime<Utc>,
    /// The refined prompt.
    pub prompt: RefinedPrompt,
    /// Attachments the prompt refers to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<DeliveredAttachment>,
    /// The repository the refinement was grounded in, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<RepositoryId>,
    /// The source's correlation metadata, carried through untouched.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl DeliveryEnvelope {
    /// The idempotency key for one attempt of one delivery.
    ///
    /// Derived rather than random so a retry after a timeout — where Refinery
    /// does not know whether the destination committed the first attempt —
    /// presents the same key and is deduplicated by the destination.
    pub fn attempt_key(delivery_id: DeliveryId, attempt: u32) -> String {
        format!("refinery:{delivery_id}:{attempt}")
    }
}

/// What a destination said in response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryReceipt {
    /// The contract version.
    pub schema_version: SchemaVersion,
    /// Whether the destination accepted the prompt.
    pub accepted: bool,
    /// The destination's identifier for what it created, when it returns one,
    /// for example an Overlord objective identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_reference: Option<String>,
    /// When the destination accepted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<DateTime<Utc>>,
    /// Why it refused, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One recorded attempt to deliver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeliveryAttempt {
    /// The delivery.
    pub delivery_id: DeliveryId,
    /// Which attempt, counting from one.
    pub attempt: u32,
    /// The idempotency key presented.
    pub idempotency_key: String,
    /// When the attempt started.
    pub started_at: DateTime<Utc>,
    /// When it finished, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// The destination's answer, when one arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<DeliveryReceipt>,
    /// How a failure was classified, which decides whether another attempt
    /// follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_class: Option<RetryClass>,
    /// The redacted failure description, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attempt_key_is_stable_for_the_same_attempt() {
        let delivery = DeliveryId::new();
        assert_eq!(
            DeliveryEnvelope::attempt_key(delivery, 2),
            DeliveryEnvelope::attempt_key(delivery, 2)
        );
        assert_ne!(
            DeliveryEnvelope::attempt_key(delivery, 2),
            DeliveryEnvelope::attempt_key(delivery, 3)
        );
        assert!(DeliveryEnvelope::attempt_key(delivery, 1).starts_with("refinery:"));
    }

    #[test]
    fn the_envelope_exposes_no_transcript_or_conversation_field() {
        // Property names only: a field description may legitimately mention
        // the transcript the refinement came from, but no field may carry it.
        let schema = serde_json::to_value(schemars::schema_for!(DeliveryEnvelope)).unwrap();
        let mut properties: Vec<String> = Vec::new();
        collect_property_names(&schema, &mut properties);
        for forbidden in [
            "transcript",
            "messages",
            "backend_run",
            "history",
            "tool_output",
        ] {
            assert!(
                !properties.iter().any(|name| name == forbidden),
                "the delivery envelope must not expose {forbidden}, found {properties:?}"
            );
        }
        assert!(properties.iter().any(|name| name == "prompt"));
    }

    fn collect_property_names(value: &serde_json::Value, into: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if key == "properties" {
                        if let Some(properties) = child.as_object() {
                            into.extend(properties.keys().cloned());
                        }
                    }
                    collect_property_names(child, into);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_property_names(item, into);
                }
            }
            _ => {}
        }
    }
}
