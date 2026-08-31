//! Stable identifiers for every durable entity.
//!
//! Identifiers are UUIDv7 so they sort by creation time, which keeps event
//! streams, job queues, and delivery attempts readable in storage order
//! without a separate sequence column. Each entity gets its own newtype so a
//! case identifier can never be passed where a delivery identifier is
//! expected; on the wire they are plain UUID strings.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The error returned when a string is not a valid identifier.
#[derive(Debug, thiserror::Error)]
#[error("invalid {kind} identifier: {value}")]
pub struct InvalidId {
    /// The identifier kind that failed to parse, for example `case`.
    pub kind: &'static str,
    /// The rejected text.
    pub value: String,
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// The entity kind this identifier names.
            pub const KIND: &'static str = $kind;

            /// Mint a new time-ordered identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wrap an existing UUID, for example one read back from storage.
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// The underlying UUID.
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self).map_err(|_| InvalidId {
                    kind: $kind,
                    value: value.to_owned(),
                })
            }
        }
    };
}

define_id!(
    /// Identifies one refinement case for its whole life.
    CaseId,
    "case"
);
define_id!(
    /// Identifies one append-only case event.
    EventId,
    "event"
);
define_id!(
    /// Identifies one imported attachment.
    AttachmentId,
    "attachment"
);
define_id!(
    /// Identifies one group of questions put to the user.
    QuestionRequestId,
    "question_request"
);
define_id!(
    /// Identifies one question inside a question request.
    QuestionId,
    "question"
);
define_id!(
    /// Identifies one registered repository root.
    RepositoryId,
    "repository"
);
define_id!(
    /// Identifies one versioned refined output.
    OutputId,
    "output"
);
define_id!(
    /// Identifies one delivery attempt series to a destination.
    DeliveryId,
    "delivery"
);
define_id!(
    /// Identifies one backend run: a provider conversation for a case.
    RunId,
    "run"
);
define_id!(
    /// Identifies one durable unit of runnable work.
    JobId,
    "job"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_time_ordered() {
        let first = CaseId::new();
        let second = CaseId::new();
        assert!(
            first < second || first.as_uuid().get_timestamp() == second.as_uuid().get_timestamp()
        );
        assert_eq!(first.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn identifiers_round_trip_through_strings() {
        let id = DeliveryId::new();
        let text = id.to_string();
        assert_eq!(DeliveryId::from_str(&text).unwrap(), id);
        assert_eq!(
            serde_json::from_str::<DeliveryId>(&format!("\"{text}\"")).unwrap(),
            id
        );
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{text}\""));
    }

    #[test]
    fn parsing_rejects_non_uuid_text() {
        let err = CaseId::from_str("not-a-uuid").unwrap_err();
        assert_eq!(err.kind, "case");
    }
}
