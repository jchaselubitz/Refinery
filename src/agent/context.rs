//! The handle a backend drives a case through.
//!
//! The product sketch gives `AgentBackend` a `start`/`answer`/`cancel` shape
//! with no way for the orchestrator to observe what happens in between. This
//! is the resolution: `start` receives a [`BackendContext`] that owns the
//! repository connector, the question service, the media store, and the
//! durable event sink, and the backend reports progress by using them rather
//! than by returning a stream. Everything a backend does — a tool call, a
//! question, a submission — lands in storage as a snapshot change paired with
//! an event, so the case's history is complete whether the backend finishes,
//! pauses for a person, or is killed mid-step.
//!
//! It also means a backend has no direct access to the filesystem, the
//! provider's file store, or the case table. What it can do is what this type
//! exposes, which is the same boundary the tool declarations describe.

use crate::domain::{
    AttachmentMetadata, AttachmentState, CaseEventPayload, CaseId, Outcome, OutputId,
    PromptValidationContext, Question, QuestionRequest, RefinedPrompt, RefinementRequest, RunId,
    UsageMetadata,
};
use crate::error::{AppError, Result};
use crate::interactions::QuestionService;
use crate::media::{ensure_case_attachments_available, GeminiFilesClient, MediaStore};
use crate::repositories::RepositoryConnector;
use crate::storage::{BackendRun, RunStatus, Storage};

use super::tools::ToolCall;

/// How many times a rejected submission may be handed back for correction.
///
/// One. A model that cannot satisfy a deterministic schema after being shown
/// exactly which fields failed is not going to satisfy it on the third attempt
/// either, and the case is more useful failed with its issues on the record
/// than spinning.
pub const CORRECTIVE_RETRIES: u32 = 1;

/// The result of a candidate submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// The prompt passed validation and the case is on its way to delivery.
    Accepted {
        /// The accepted output.
        output_id: OutputId,
    },
    /// The prompt failed validation and the backend has a retry left. The
    /// issues are phrased for the model, not for a log.
    Rejected {
        /// The rejected output, recorded for the audit trail.
        output_id: OutputId,
        /// What was wrong, one issue per entry.
        issues: Vec<String>,
    },
    /// The prompt failed validation with no retry left; the case has failed.
    Exhausted {
        /// The rejected output.
        output_id: OutputId,
        /// What was wrong the second time.
        issues: Vec<String>,
    },
}

/// The answers to one question request, ready to hand back to a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerReplay {
    /// The request that was answered.
    pub question_request_id: crate::domain::QuestionRequestId,
    /// The function result to append to the conversation.
    pub result: serde_json::Value,
}

/// The provider-side media handles a case needs to reference its attachments.
#[derive(Clone, Debug)]
pub struct MediaContext {
    /// The local original of every imported attachment.
    pub store: MediaStore,
    /// The provider's file store, which holds a cache of those originals.
    pub files: GeminiFilesClient,
}

/// Everything a backend is allowed to reach, bound to one case.
#[derive(Clone, Debug)]
pub struct BackendContext {
    storage: Storage,
    questions: QuestionService,
    case_id: CaseId,
    request: RefinementRequest,
    repository: Option<RepositoryConnector>,
    media: Option<MediaContext>,
}

impl BackendContext {
    /// Assemble the handle for one case.
    ///
    /// The repository connector is built here, once, from the registration the
    /// request names. A case that names a repository which has since been
    /// unregistered fails at this point rather than halfway through a tool
    /// loop, which is the difference between a clear failure and a confusing
    /// one.
    pub async fn load(
        storage: Storage,
        case_id: CaseId,
        media: Option<MediaContext>,
    ) -> Result<Self> {
        let request = storage.case_request(case_id).await?;
        let repository = match request.repository {
            Some(id) => {
                let repository = storage.repository(id).await?;
                Some(RepositoryConnector::new(repository, storage.clone()))
            }
            None => None,
        };
        Ok(Self {
            questions: QuestionService::new(storage.clone()),
            storage,
            case_id,
            request,
            repository,
            media,
        })
    }

    /// The case this context belongs to.
    pub fn case_id(&self) -> CaseId {
        self.case_id
    }

    /// The request exactly as it was submitted.
    pub fn request(&self) -> &RefinementRequest {
        &self.request
    }

    /// The case's repository connector, when it has one.
    pub fn repository(&self) -> Option<&RepositoryConnector> {
        self.repository.as_ref()
    }

    /// The durable store, for a backend that needs its own run bookkeeping.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Every attachment the provider currently holds a usable copy of.
    ///
    /// Uploading and re-uploading happen here rather than at case preparation,
    /// because a provider's copy expires on its own schedule and a case that
    /// waited overnight for an answer will routinely come back to files that
    /// are gone. The local original is authoritative, so an expiry costs one
    /// upload, never a case.
    pub async fn available_attachments(&self) -> Result<Vec<AttachmentMetadata>> {
        let Some(media) = self.media.as_ref() else {
            // No media handles means no provider file store to reference. Any
            // attachment on the case is still recorded and still visible in the
            // case's history; it simply cannot be shown to the model.
            return Ok(Vec::new());
        };
        ensure_case_attachments_available(
            &self.storage,
            &media.store,
            &media.files,
            self.case_id,
            crate::media::lifecycle::now(),
        )
        .await
    }

    /// Run one repository tool.
    ///
    /// A case with no repository refuses every repository tool by policy, which
    /// is the same answer the connector would give for a path outside its root:
    /// the model gets a reason and can carry on without it.
    pub async fn call_tool(&self, call: ToolCall) -> Result<serde_json::Value> {
        let Some(connector) = self.repository.as_ref() else {
            return Err(AppError::PolicyDenied {
                message: "this case has no registered repository, so repository tools are \
                          unavailable"
                    .into(),
            });
        };
        super::tools::dispatch_repository(connector, self.case_id, call).await
    }

    /// Raise a question and pause the case.
    pub async fn ask_user(
        &self,
        prompt: impl Into<String>,
        questions: Vec<Question>,
    ) -> Result<QuestionRequest> {
        self.questions.ask(self.case_id, prompt, questions).await
    }

    /// The answered request and its answers, shaped for replay.
    ///
    /// A resumed run has to hand the model a function result for the
    /// `ask_user` call it made before it stopped. That result is built here,
    /// from the durable rows, so it is identical whether the answer arrived a
    /// second later or after a restart three days on.
    pub async fn answers_for_replay(&self) -> Result<Option<AnswerReplay>> {
        let Some(answers) = self.questions.latest_answers(self.case_id).await? else {
            return Ok(None);
        };
        let request = self
            .storage
            .question_request(answers.question_request_id)
            .await?;
        let rendered: Vec<serde_json::Value> = request
            .questions
            .iter()
            .map(|question| {
                serde_json::json!({
                    "question": question.label,
                    "answer": answers
                        .answer_for(question.id)
                        .map(|answer| render_answer(&answer.value)),
                })
            })
            .collect();
        Ok(Some(AnswerReplay {
            question_request_id: answers.question_request_id,
            result: serde_json::json!({ "answers": rendered }),
        }))
    }

    /// Validate and record a candidate refined prompt.
    ///
    /// Validation is the deterministic contract check, run before anything is
    /// accepted, and its issues are handed back to the model verbatim: they
    /// name fields and say what is wrong, which is exactly the correction a
    /// model can act on.
    pub async fn submit(&self, prompt: RefinedPrompt) -> Result<Submission> {
        let context = self.validation_context().await?;
        let issues: Vec<String> = match prompt.validate(&context) {
            Ok(()) => Vec::new(),
            Err(report) => report.issues.iter().map(ToString::to_string).collect(),
        };
        let output_id = OutputId::new();
        let valid = issues.is_empty();
        self.storage
            .record_output(self.case_id, output_id, &prompt, valid, &issues)
            .await?;
        if valid {
            self.storage.accept_output(self.case_id, output_id).await?;
            return Ok(Submission::Accepted { output_id });
        }
        let already_rejected = self.storage.rejected_output_count(self.case_id).await?;
        if already_rejected > CORRECTIVE_RETRIES {
            return Ok(Submission::Exhausted { output_id, issues });
        }
        self.storage
            .reject_output_for_retry(self.case_id, output_id)
            .await?;
        Ok(Submission::Rejected { output_id, issues })
    }

    /// Open or resume the case's provider conversation.
    pub async fn open_run(&self, backend_id: &str) -> Result<BackendRun> {
        self.storage.open_run(self.case_id, backend_id).await
    }

    /// Persist the conversation after one provider step.
    pub async fn save_step(
        &self,
        run: &BackendRun,
        history: &serde_json::Value,
        usage: Option<UsageMetadata>,
        outcome: Outcome,
    ) -> Result<u32> {
        self.storage
            .save_run_step(run, history, usage, outcome)
            .await
    }

    /// Close the conversation.
    pub async fn finish_run(
        &self,
        run_id: RunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<()> {
        self.storage.finish_run(run_id, status, error).await
    }

    /// Fail the case with a person-readable, secret-free reason.
    pub async fn fail(&self, reason: impl Into<String>) -> Result<()> {
        self.storage.fail_case(self.case_id, reason).await
    }

    /// Record an observation on the case's history without changing its state.
    pub async fn note(&self, message: impl Into<String>) -> Result<()> {
        self.storage
            .record_note(
                self.case_id,
                CaseEventPayload::Note {
                    message: message.into(),
                },
            )
            .await
    }

    async fn validation_context(&self) -> Result<PromptValidationContext> {
        let unresolved_required_questions = self
            .questions
            .pending(self.case_id)
            .await?
            .map(|request| {
                request
                    .questions
                    .iter()
                    .filter(|question| question.required)
                    .map(|question| question.id)
                    .collect()
            })
            .unwrap_or_default();
        let known_attachments = self
            .storage
            .case_attachments(self.case_id)
            .await?
            .into_iter()
            .filter(|stored| stored.metadata.state != AttachmentState::Rejected)
            .map(|stored| stored.metadata.id)
            .collect();
        Ok(PromptValidationContext {
            unresolved_required_questions,
            known_attachments,
        })
    }
}

/// Render one answer as the sentence a model reads.
fn render_answer(value: &crate::domain::AnswerValue) -> String {
    match value {
        crate::domain::AnswerValue::Text { text } => text.clone(),
        crate::domain::AnswerValue::Choice { value } => value.clone(),
        crate::domain::AnswerValue::Choices { values } => values.join(", "),
    }
}
