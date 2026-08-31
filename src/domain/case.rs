//! Case state, lifecycle transitions, and the append-only event record.
//!
//! Two things live here and they are deliberately different. [`CaseState`] and
//! [`CaseTransition`] are the lifecycle: a small closed vocabulary the state
//! machine in `cases::state_machine` reasons over. [`CaseEvent`] is the record:
//! every lifecycle change *plus* the progress and audit facts the product
//! promises — repository reads, provider calls, questions, and deliveries —
//! written append-only beside the case snapshot.
//!
//! Keeping them separate is what lets the state machine stay a pure, totally
//! enumerable function while the event log stays rich enough to reconstruct
//! what happened without full event sourcing.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::ids::{
    AttachmentId, CaseId, DeliveryId, EventId, OutputId, QuestionRequestId, RunId,
};
use crate::domain::AttachmentState;
use crate::domain::SchemaVersion;
use crate::error::RetryClass;

/// Where a case stands.
///
/// The happy path is `received -> preparing -> running -> validating -> ready
/// -> delivering -> completed`, with `running <-> awaiting_answer` repeating as
/// often as the agent needs answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaseState {
    /// Accepted and durable; nothing has been done with it yet.
    Received,
    /// Importing attachments, resolving the repository, selecting a backend.
    Preparing,
    /// The agent backend is working.
    Running,
    /// A question is outstanding; no job holds the case.
    AwaitingAnswer,
    /// A candidate output is being validated.
    Validating,
    /// A valid output exists and delivery has not been attempted.
    Ready,
    /// Delivery is in progress, including between scheduled retries.
    Delivering,
    /// The destination accepted the refined prompt.
    Completed,
    /// The case stopped without delivering. Any output produced is preserved,
    /// so a user may retry delivery from here.
    Failed,
    /// Stopped at the user's request.
    Cancelled,
}

impl CaseState {
    /// The state every case starts in.
    pub const INITIAL: CaseState = CaseState::Received;

    /// Every state, for exhaustive table tests.
    pub const ALL: &'static [CaseState] = &[
        CaseState::Received,
        CaseState::Preparing,
        CaseState::Running,
        CaseState::AwaitingAnswer,
        CaseState::Validating,
        CaseState::Ready,
        CaseState::Delivering,
        CaseState::Completed,
        CaseState::Failed,
        CaseState::Cancelled,
    ];

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CaseState::Received => "received",
            CaseState::Preparing => "preparing",
            CaseState::Running => "running",
            CaseState::AwaitingAnswer => "awaiting_answer",
            CaseState::Validating => "validating",
            CaseState::Ready => "ready",
            CaseState::Delivering => "delivering",
            CaseState::Completed => "completed",
            CaseState::Failed => "failed",
            CaseState::Cancelled => "cancelled",
        }
    }

    /// Whether the case has stopped moving on its own.
    ///
    /// `Failed` counts: nothing Refinery does advances it. A user may still
    /// retry delivery from `Failed`, which is the single documented way a
    /// terminal state is left, and it always requires a person.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CaseState::Completed | CaseState::Failed | CaseState::Cancelled
        )
    }

    /// Whether a background job may be working the case in this state.
    ///
    /// `AwaitingAnswer` is the interesting one: the case is live but no job
    /// holds it, which is what "waits without holding an HTTP request or
    /// process lock" means in practice.
    pub fn has_active_work(self) -> bool {
        matches!(
            self,
            CaseState::Preparing
                | CaseState::Running
                | CaseState::Validating
                | CaseState::Delivering
        )
    }

    /// Whether startup recovery should resume this case by enqueueing work.
    pub fn resumes_after_restart(self) -> bool {
        matches!(
            self,
            CaseState::Received
                | CaseState::Preparing
                | CaseState::Running
                | CaseState::Validating
                | CaseState::Ready
                | CaseState::Delivering
        )
    }
}

impl std::fmt::Display for CaseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A thing that happens to a case and may move it to another state.
///
/// This is the input alphabet of the state machine. It is intentionally small
/// and closed: anything that is only an observation, not a lifecycle change,
/// is a [`CaseEventPayload`] variant instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaseTransition {
    /// Preparation started: attachments, repository, backend selection.
    PreparationStarted,
    /// Preparation finished and the backend began work.
    BackendStarted,
    /// The agent asked the user something and stopped.
    QuestionRaised {
        /// The outstanding request.
        question_request_id: QuestionRequestId,
    },
    /// Answers arrived and the case resumes. This edge repeats.
    AnswersApplied {
        /// The request that was answered.
        question_request_id: QuestionRequestId,
    },
    /// The agent submitted a candidate refined prompt.
    OutputSubmitted {
        /// The candidate output.
        output_id: OutputId,
    },
    /// The candidate passed deterministic validation.
    ValidationSucceeded {
        /// The accepted output.
        output_id: OutputId,
    },
    /// The candidate failed validation and the agent gets its one corrective
    /// retry, so the case returns to `running`.
    ValidationRejectedWithRetry {
        /// The rejected output.
        output_id: OutputId,
    },
    /// A delivery attempt began. Legal from `ready` for the first attempt and
    /// from `failed` when a person retries.
    DeliveryStarted {
        /// The delivery this attempt belongs to.
        delivery_id: DeliveryId,
    },
    /// An attempt failed and another is scheduled; the case stays in
    /// `delivering`.
    DeliveryAttemptFailed {
        /// The delivery that failed.
        delivery_id: DeliveryId,
        /// How the failure was classified.
        retry_class: RetryClass,
    },
    /// The destination accepted the prompt.
    DeliverySucceeded {
        /// The successful delivery.
        delivery_id: DeliveryId,
    },
    /// Delivery gave up: retries exhausted or a non-retryable failure. The
    /// output is preserved for a user-initiated retry.
    DeliveryExhausted {
        /// The delivery that gave up.
        delivery_id: DeliveryId,
    },
    /// The case failed for any other reason.
    Failed {
        /// A person-readable, secret-free reason.
        reason: String,
    },
    /// The user cancelled the case.
    Cancelled {
        /// Why, when the user or caller said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl CaseTransition {
    /// The wire spelling, for logs and metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            CaseTransition::PreparationStarted => "preparation_started",
            CaseTransition::BackendStarted => "backend_started",
            CaseTransition::QuestionRaised { .. } => "question_raised",
            CaseTransition::AnswersApplied { .. } => "answers_applied",
            CaseTransition::OutputSubmitted { .. } => "output_submitted",
            CaseTransition::ValidationSucceeded { .. } => "validation_succeeded",
            CaseTransition::ValidationRejectedWithRetry { .. } => "validation_rejected_with_retry",
            CaseTransition::DeliveryStarted { .. } => "delivery_started",
            CaseTransition::DeliveryAttemptFailed { .. } => "delivery_attempt_failed",
            CaseTransition::DeliverySucceeded { .. } => "delivery_succeeded",
            CaseTransition::DeliveryExhausted { .. } => "delivery_exhausted",
            CaseTransition::Failed { .. } => "failed",
            CaseTransition::Cancelled { .. } => "cancelled",
        }
    }
}

/// How a recorded operation turned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It worked.
    Succeeded,
    /// It did not.
    Failed,
    /// Policy refused it before it ran.
    Denied,
}

/// Provider usage recorded for a run step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct UsageMetadata {
    /// Tokens in the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Tokens in the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Tokens the provider billed in total, when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

/// What an event records.
///
/// [`CaseEventPayload::StateChanged`] carries the lifecycle; every other
/// variant is a progress or audit fact that leaves the state alone. The audit
/// variants exist because the security model promises "per-case audit events
/// for repository reads, provider calls, questions, and deliveries", and an
/// audit trail that is not part of the contract is one that drifts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaseEventPayload {
    /// The case was accepted.
    CaseReceived {
        /// The submitting system, as a wire string.
        source_system: String,
        /// The source's request identifier.
        request_id: String,
        /// How many attachments arrived.
        attachment_count: usize,
        /// Whether the case names a repository.
        has_repository: bool,
    },
    /// The case moved from one state to another.
    StateChanged {
        /// The state before.
        from: CaseState,
        /// The state after.
        to: CaseState,
        /// What caused the move.
        transition: CaseTransition,
    },
    /// An attachment was imported into the case media store.
    AttachmentImported {
        /// The attachment.
        attachment_id: AttachmentId,
        /// Its measured media type.
        media_type: String,
        /// Its measured size.
        size_bytes: u64,
    },
    /// An attachment's provider upload state changed.
    AttachmentStateChanged {
        /// The attachment.
        attachment_id: AttachmentId,
        /// The new state.
        state: AttachmentState,
    },
    /// A repository tool ran. Arguments are recorded as a digest, not
    /// verbatim, so the audit trail cannot itself become a copy of repository
    /// content.
    RepositoryToolInvoked {
        /// The tool name.
        tool: String,
        /// A digest of the arguments.
        arguments_digest: String,
        /// Bytes returned to the model.
        result_bytes: usize,
        /// How it turned out.
        outcome: Outcome,
    },
    /// One provider request completed.
    ProviderCallCompleted {
        /// The backend identifier, for example `gemini`.
        backend_id: String,
        /// The run the call belongs to.
        run_id: RunId,
        /// Which step of the conversation.
        step: u32,
        /// Reported usage, when the provider supplies it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageMetadata>,
        /// How it turned out.
        outcome: Outcome,
    },
    /// The agent raised a question request.
    QuestionRequested {
        /// The request.
        question_request_id: QuestionRequestId,
        /// How many questions it contains.
        question_count: usize,
    },
    /// A question request was sent to a source's callback.
    QuestionForwarded {
        /// The request.
        question_request_id: QuestionRequestId,
        /// Where it went, for example `overlord` or `local_ui`.
        target: String,
        /// How it turned out.
        outcome: Outcome,
    },
    /// Answers were validated and stored.
    AnswersRecorded {
        /// The request answered.
        question_request_id: QuestionRequestId,
        /// How many answers arrived.
        answer_count: usize,
    },
    /// A candidate output was recorded with its validation result.
    OutputRecorded {
        /// The output.
        output_id: OutputId,
        /// Whether it passed deterministic validation.
        valid: bool,
        /// The issues found, when it did not.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        issues: Vec<String>,
    },
    /// One delivery attempt finished.
    DeliveryAttempted {
        /// The delivery.
        delivery_id: DeliveryId,
        /// The destination kind.
        destination: String,
        /// Which attempt this was, counting from one.
        attempt: u32,
        /// How it turned out.
        outcome: Outcome,
        /// How a failure was classified.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_class: Option<RetryClass>,
    },
    /// Something worth recording that is not any of the above.
    Note {
        /// A secret-free sentence.
        message: String,
    },
}

impl CaseEventPayload {
    /// The wire spelling of the event type.
    pub fn type_str(&self) -> &'static str {
        match self {
            CaseEventPayload::CaseReceived { .. } => "case_received",
            CaseEventPayload::StateChanged { .. } => "state_changed",
            CaseEventPayload::AttachmentImported { .. } => "attachment_imported",
            CaseEventPayload::AttachmentStateChanged { .. } => "attachment_state_changed",
            CaseEventPayload::RepositoryToolInvoked { .. } => "repository_tool_invoked",
            CaseEventPayload::ProviderCallCompleted { .. } => "provider_call_completed",
            CaseEventPayload::QuestionRequested { .. } => "question_requested",
            CaseEventPayload::QuestionForwarded { .. } => "question_forwarded",
            CaseEventPayload::AnswersRecorded { .. } => "answers_recorded",
            CaseEventPayload::OutputRecorded { .. } => "output_recorded",
            CaseEventPayload::DeliveryAttempted { .. } => "delivery_attempted",
            CaseEventPayload::Note { .. } => "note",
        }
    }

    /// The state this event moved the case to, when it moved it at all.
    pub fn resulting_state(&self) -> Option<CaseState> {
        match self {
            CaseEventPayload::StateChanged { to, .. } => Some(*to),
            _ => None,
        }
    }
}

/// One append-only record in a case's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaseEvent {
    /// The contract version.
    pub schema_version: SchemaVersion,
    /// The event's identifier.
    pub id: EventId,
    /// The case it belongs to.
    pub case_id: CaseId,
    /// Position in the case's history, counting from one. Explicit rather than
    /// implied by timestamp, so clients can page and resume a stream exactly.
    pub sequence: u64,
    /// When it happened, in UTC.
    pub occurred_at: DateTime<Utc>,
    /// What happened.
    #[serde(flatten)]
    pub payload: CaseEventPayload,
}

impl CaseEvent {
    /// Build an event for a case at a given sequence position.
    pub fn new(
        case_id: CaseId,
        sequence: u64,
        occurred_at: DateTime<Utc>,
        payload: CaseEventPayload,
    ) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            id: EventId::new(),
            case_id,
            sequence,
            occurred_at,
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_exactly_the_three_stopping_points() {
        let terminal: Vec<CaseState> = CaseState::ALL
            .iter()
            .copied()
            .filter(|state| state.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            vec![
                CaseState::Completed,
                CaseState::Failed,
                CaseState::Cancelled
            ]
        );
    }

    #[test]
    fn awaiting_answer_is_live_but_holds_no_job() {
        assert!(!CaseState::AwaitingAnswer.is_terminal());
        assert!(!CaseState::AwaitingAnswer.has_active_work());
        assert!(!CaseState::AwaitingAnswer.resumes_after_restart());
    }

    #[test]
    fn recovery_resumes_every_non_terminal_state_except_awaiting_answer() {
        for state in CaseState::ALL {
            let expected = !state.is_terminal() && *state != CaseState::AwaitingAnswer;
            assert_eq!(state.resumes_after_restart(), expected, "{state}");
        }
    }

    #[test]
    fn an_event_serializes_with_its_type_flattened_onto_the_envelope() {
        let event = CaseEvent::new(
            CaseId::new(),
            1,
            Utc::now(),
            CaseEventPayload::StateChanged {
                from: CaseState::Received,
                to: CaseState::Preparing,
                transition: CaseTransition::PreparationStarted,
            },
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "state_changed");
        assert_eq!(json["from"], "received");
        assert_eq!(json["transition"]["kind"], "preparation_started");
        assert_eq!(json["sequence"], 1);
        let parsed: CaseEvent = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn state_wire_spellings_match_the_serde_representation() {
        for state in CaseState::ALL {
            let json = serde_json::to_string(state).unwrap();
            assert_eq!(json, format!("\"{}\"", state.as_str()));
        }
    }
}
