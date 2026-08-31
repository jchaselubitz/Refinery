//! The agent backend boundary.
//!
//! `AgentBackend` is the central extension point, not merely a model client.
//! [`AgentBackend::start`] receives a [`BackendContext`] handle owning the
//! repository tools, the question service, and the event sink; the backend
//! drives the case through that handle rather than returning a stream. Stage 1
//! ships [`gemini`] as the sole implementation; later backends live beside it
//! until their dependency trees justify separate crates.
//!
//! The product sketch also gives the trait an `answer` method. There is none
//! here, deliberately. An answer arrives through the API, is validated and
//! stored by the question service, and schedules a resume job; the backend
//! then discovers it by calling `start` again on the same persisted run. That
//! keeps a backend from having to be alive — or even to be the same process —
//! when a person finally replies, which is the whole point of pausing a case
//! in durable storage rather than holding a task open.

use async_trait::async_trait;

use crate::domain::{BackendCapabilities, OutputId, QuestionRequestId, RunId};
use crate::error::Result;

pub mod connection_test;
pub mod context;
pub mod gemini;
pub mod instruction;
pub mod tools;

pub use connection_test::{test_gemini, ProviderConnection, GEMINI_API_BASE};
pub use context::{AnswerReplay, BackendContext, MediaContext, Submission, CORRECTIVE_RETRIES};
pub use gemini::GeminiBackend;

/// How a backend's turn on a case ended.
///
/// Every variant is a place the case can legitimately rest between jobs, which
/// is why there is no "still going": a backend either finished, stopped for a
/// person, or failed, and the job that called it completes in all three cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// A valid refined prompt was accepted; the case is on its way to delivery.
    Completed {
        /// The accepted output.
        output_id: OutputId,
    },
    /// The backend asked the user something and the case is paused. The next
    /// answer schedules the resume.
    AwaitingAnswer {
        /// The outstanding request.
        question_request_id: QuestionRequestId,
    },
    /// The backend gave up and the case has been failed with this reason.
    Failed {
        /// A person-readable, secret-free reason.
        reason: String,
    },
}

/// One way of turning a case's inputs into a refined prompt.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// The stable identifier recorded on runs and events, for example `gemini`.
    fn backend_id(&self) -> &'static str;

    /// What this backend can do, for the pre-start capability check.
    async fn capabilities(&self) -> Result<BackendCapabilities>;

    /// Drive the case until it finishes, pauses for a person, or fails.
    ///
    /// Called for a fresh case and again for every resume. A backend must work
    /// out which it is from the persisted run rather than from an argument,
    /// because a restart between the two looks exactly the same from here.
    async fn start(&self, context: &BackendContext) -> Result<RunOutcome>;

    /// Abandon a run. Local state is already durable, so this only has to
    /// release whatever the provider is holding.
    async fn cancel(&self, run: RunId) -> Result<()>;
}
