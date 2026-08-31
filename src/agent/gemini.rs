//! The Gemini agent backend: the Stage 1 implementation of [`AgentBackend`].
//!
//! Runs the tool loop — the five read-only repository tools plus `ask_user`
//! and `submit_refined_prompt` — over iterative `generateContent` calls, with
//! the full conversation history persisted to `backend_runs` after every step.
//! Gemini's API is stateless per request, so resuming a case replays that
//! persisted history with the pending function response appended rather than
//! relying on a provider-side session.
//!
//! Three rules shape the loop and are worth stating outright:
//!
//! - **The history is the conversation.** Nothing lives only in memory. A step
//!   is not complete until its history is committed, so a process killed
//!   between the provider's reply and the next request resumes from the reply
//!   rather than repeating the call.
//! - **`ask_user` must be called alone.** When the model asks a question in the
//!   same turn as other tool calls, the question is refused with a reason and
//!   the other calls are answered normally. Honouring it would mean suspending
//!   with sibling calls unanswered, and a resumed conversation whose function
//!   responses do not line up with its calls is one the provider rejects.
//! - **A failure is classified, never interpreted.** Every provider error maps
//!   to a retry class at this boundary; the job runner acts on the class and
//!   never on the text.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::secret::SecretString;
use crate::domain::{
    AttachmentKind, AttachmentMetadata, BackendCapabilities, Outcome, RunId, SchemaVersion,
    UsageMetadata,
};
use crate::error::{AppError, Result, RetryClass};
use crate::storage::RunStatus;

use super::connection_test::{classify_failure, GEMINI_API_BASE};
use super::instruction::{case_briefing, repository_briefing, SYSTEM_INSTRUCTION};
use super::tools::{self, ToolCall, ASK_USER, SUBMIT_REFINED_PROMPT};
use super::{AgentBackend, BackendContext, RunOutcome, Submission};

/// The identifier recorded on every run and provider-call event.
pub const BACKEND_ID: &str = "gemini";

/// How many provider calls one job may make before giving up.
///
/// A refinement that has not converged in this many steps is looping, and the
/// bound is what keeps a loop from becoming an unbounded bill. It counts steps
/// within one job: a case that pauses for an answer and resumes gets a fresh
/// budget, because a human round trip is evidence of progress rather than of
/// spinning.
pub const MAX_STEPS_PER_RUN: u32 = 24;

/// How many turns the model may spend replying with prose instead of calling a
/// tool before the case is failed. Models occasionally answer a tool-use turn
/// with text; one reminder is worth sending, a second is a stuck conversation.
const MAX_NUDGES: u32 = 1;

/// How long one `generateContent` call may take.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// The Stage 1 backend.
#[derive(Clone, Debug)]
pub struct GeminiBackend {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: SecretString,
    max_steps: u32,
}

impl GeminiBackend {
    /// Build a backend for one model and credential.
    pub fn new(model: impl Into<String>, api_key: SecretString) -> Result<Self> {
        Self::with_base_url(GEMINI_API_BASE, model, api_key)
    }

    /// Build a backend against a specific API base, for tests and proxies.
    pub fn with_base_url(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: SecretString,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|source| AppError::Internal(source.into()))?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            api_key,
            max_steps: MAX_STEPS_PER_RUN,
        })
    }

    /// Lower the per-job step budget, for tests that assert the bound.
    pub fn with_max_steps(mut self, max_steps: u32) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// The model this backend drives.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// One `generateContent` call.
    ///
    /// The key travels in a header, never in the URL, so it cannot reach a
    /// proxy log or an error message that echoes the request line.
    async fn generate(&self, request: &GenerateContentRequest) -> Result<GenerateContentResponse> {
        let url = format!(
            "{}/models/{}:generateContent",
            self.base_url,
            self.model.trim_start_matches("models/")
        );
        let response = self
            .http
            .post(url)
            .header("x-goog-api-key", self.api_key.expose())
            .json(request)
            .send()
            .await
            .map_err(|source| AppError::Upstream {
                service: BACKEND_ID,
                message: crate::diagnostics::redaction::redact(&format!(
                    "could not reach the provider: {source}"
                ))
                .into_owned(),
                retry: RetryClass::Retryable,
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(classify_failure(status, &body));
        }
        serde_json::from_str(&body).map_err(|source| AppError::Upstream {
            service: BACKEND_ID,
            message: format!("the provider returned an unreadable response: {source}"),
            retry: RetryClass::Retryable,
        })
    }

    fn request_for(&self, context: &BackendContext, history: &[Content]) -> GenerateContentRequest {
        GenerateContentRequest {
            system_instruction: Some(Content::system(SYSTEM_INSTRUCTION)),
            contents: history.to_vec(),
            tools: vec![Tools {
                function_declarations: tools::declarations(context.repository().is_some()),
            }],
        }
    }

    /// Build the opening turn: the briefing, the repository note, and a part
    /// per usable attachment.
    async fn opening_turn(&self, context: &BackendContext) -> Result<Vec<Content>> {
        let attachments = context.available_attachments().await?;
        let mut parts = vec![Part::text(case_briefing(context.request(), &attachments))];
        for attachment in &attachments {
            if let Some(part) = self.attachment_part(attachment) {
                parts.push(part);
            }
        }
        if let Some(connector) = context.repository() {
            parts.push(Part::text(repository_briefing(
                &connector.repository().label(),
            )));
        }
        Ok(vec![Content {
            role: Some("user".into()),
            parts,
        }])
    }

    /// Reference one uploaded attachment.
    ///
    /// The provider addresses an uploaded file by a URI under the same API
    /// base its name was minted against, so the URI is reconstructed rather
    /// than stored: a `files/abc` name is durable across a base-URL change,
    /// while a full URI recorded months ago may not be.
    fn attachment_part(&self, attachment: &AttachmentMetadata) -> Option<Part> {
        if !matches!(
            attachment.kind,
            AttachmentKind::Image | AttachmentKind::Video | AttachmentKind::File
        ) {
            return None;
        }
        let name = attachment.provider_file_name.as_deref()?;
        Some(Part {
            file_data: Some(FileData {
                mime_type: Some(attachment.media_type.clone()),
                file_uri: format!(
                    "{}/files/{}",
                    self.base_url,
                    name.trim_start_matches("files/")
                ),
            }),
            ..Part::default()
        })
    }
}

#[async_trait]
impl AgentBackend for GeminiBackend {
    fn backend_id(&self) -> &'static str {
        BACKEND_ID
    }

    /// Reported without a network call.
    ///
    /// These are properties of the backend's design, not of a particular
    /// credential: the tool loop, the structured submission, and the replayed
    /// history are Refinery's own doing, and they are true before any key
    /// exists. Whether the configured credential and model actually work is a
    /// different question, answered by the provider connection test that setup
    /// and `doctor` run.
    async fn capabilities(&self) -> Result<BackendCapabilities> {
        Ok(BackendCapabilities {
            schema_version: SchemaVersion::CURRENT,
            backend_id: BACKEND_ID.into(),
            model: Some(self.model.clone()),
            text_input: true,
            image_input: true,
            native_video_input: true,
            tool_calling: true,
            structured_output: true,
            resumable_conversation: true,
            repository_runtime: false,
        })
    }

    async fn start(&self, context: &BackendContext) -> Result<RunOutcome> {
        let mut run = context.open_run(BACKEND_ID).await?;
        let mut history: Vec<Content> =
            serde_json::from_value(run.history.clone()).map_err(|source| AppError::Storage {
                message: format!("the persisted conversation could not be read: {source}"),
                retry: RetryClass::NonRetryable,
            })?;

        if history.is_empty() {
            history = self.opening_turn(context).await?;
        } else if outstanding_question(&history) {
            // The run stopped on an `ask_user` call. Whatever happened in
            // between — a minute, a restart, three days — the model sees only
            // the function result it was waiting for.
            let Some(replay) = context.answers_for_replay().await? else {
                return Err(AppError::Invalid {
                    message: "the run is waiting on an answer that was never recorded".into(),
                });
            };
            history.push(Content::function_response(ASK_USER, replay.result));
        }

        let mut nudges = 0;
        for _ in 0..self.max_steps {
            let request = self.request_for(context, &history);
            let response = match self.generate(&request).await {
                Ok(response) => response,
                Err(error) => {
                    // Record the attempt before propagating: a case whose
                    // history shows nothing between two retries is one nobody
                    // can diagnose. The job runner decides what happens next
                    // from the error's class.
                    context
                        .save_step(&run, &to_value(&history)?, None, Outcome::Failed)
                        .await?;
                    return Err(error);
                }
            };

            let usage = response.usage();
            let Some(content) = response.into_content() else {
                context
                    .save_step(&run, &to_value(&history)?, usage, Outcome::Failed)
                    .await?;
                return Err(AppError::Upstream {
                    service: BACKEND_ID,
                    message: "the provider returned no candidate content".into(),
                    retry: RetryClass::Retryable,
                });
            };
            history.push(content.clone());
            run.step = context
                .save_step(&run, &to_value(&history)?, usage, Outcome::Succeeded)
                .await?;

            let calls = content.function_calls();
            if calls.is_empty() {
                nudges += 1;
                if nudges > MAX_NUDGES {
                    let reason = "the model replied with text instead of submitting a refined \
                                  prompt, twice"
                        .to_owned();
                    context.fail(reason.clone()).await?;
                    context
                        .finish_run(run.id, RunStatus::Failed, Some(&reason))
                        .await?;
                    return Ok(RunOutcome::Failed { reason });
                }
                history.push(Content::user(
                    "That reply was not a tool call, so nothing was recorded. Continue by \
                     calling a tool: use the repository tools or ask_user if you still need \
                     information, and submit_refined_prompt when you are ready to finish.",
                ));
                continue;
            }

            // A question is only honoured when it is the whole turn; see the
            // module documentation for why.
            let asked_alone = calls.len() == 1 && calls[0].name == ASK_USER;
            let mut responses = Vec::with_capacity(calls.len());
            for call in &calls {
                if call.name == ASK_USER {
                    if asked_alone {
                        return self
                            .suspend_for_answer(context, &mut run, history, call)
                            .await;
                    }
                    responses.push(Part::function_response_part(
                        ASK_USER,
                        json!({
                            "error": "ask_user must be the only tool call in a turn. Call it on \
                                      its own once you know what to ask."
                        }),
                    ));
                    continue;
                }
                match self.run_call(context, call).await? {
                    CallResult::Response(part) => responses.push(part),
                    CallResult::Finished(outcome) => {
                        context
                            .finish_run(
                                run.id,
                                match &outcome {
                                    RunOutcome::Completed { .. } => RunStatus::Finished,
                                    _ => RunStatus::Failed,
                                },
                                None,
                            )
                            .await?;
                        return Ok(outcome);
                    }
                }
            }
            history.push(Content {
                role: Some("user".into()),
                parts: responses,
            });
        }

        let reason = format!(
            "refinement did not converge within {} provider steps",
            self.max_steps
        );
        context.fail(reason.clone()).await?;
        context
            .finish_run(run.id, RunStatus::Failed, Some(&reason))
            .await?;
        Ok(RunOutcome::Failed { reason })
    }

    async fn cancel(&self, _run: RunId) -> Result<()> {
        // Nothing to release: `generateContent` holds no server-side session,
        // and the case's own cancellation is a storage transition owned by the
        // orchestrator rather than by the backend.
        Ok(())
    }
}

/// What handling one function call produced.
enum CallResult {
    /// A function result to send back in the next turn.
    Response(Part),
    /// The run is over.
    Finished(RunOutcome),
}

impl GeminiBackend {
    /// Persist the question and stop, leaving the model's call unanswered in
    /// the history so the resume knows exactly what it owes a result for.
    async fn suspend_for_answer(
        &self,
        context: &BackendContext,
        run: &mut crate::storage::BackendRun,
        history: Vec<Content>,
        call: &FunctionCall,
    ) -> Result<RunOutcome> {
        let ToolCall::AskUser(input) = ToolCall::parse(ASK_USER, &call.args)? else {
            unreachable!("the call was matched by name");
        };
        let (prompt, questions) = input.into_questions();
        let request = context.ask_user(prompt, questions).await?;
        run.step = context
            .save_step(run, &to_value(&history)?, None, Outcome::Succeeded)
            .await?;
        Ok(RunOutcome::AwaitingAnswer {
            question_request_id: request.id,
        })
    }

    /// Run one non-question call and turn it into a function result.
    async fn run_call(&self, context: &BackendContext, call: &FunctionCall) -> Result<CallResult> {
        let parsed = match ToolCall::parse(&call.name, &call.args) {
            Ok(parsed) => parsed,
            // A mistyped name or a malformed argument bag is the model's
            // mistake to correct, not a reason to fail a case. It gets the
            // reason and another turn.
            Err(error) => {
                return Ok(CallResult::Response(Part::function_response_part(
                    &call.name,
                    json!({ "error": error.detail() }),
                )))
            }
        };

        if parsed.needs_repository() {
            let result = match context.call_tool(parsed).await {
                Ok(value) => value,
                Err(error) => json!({ "error": error.detail() }),
            };
            return Ok(CallResult::Response(Part::function_response_part(
                &call.name, result,
            )));
        }

        let ToolCall::Submit(prompt) = parsed else {
            return Ok(CallResult::Response(Part::function_response_part(
                &call.name,
                json!({ "error": "that tool is not available on this case" }),
            )));
        };

        match context.submit(*prompt).await? {
            Submission::Accepted { output_id } => {
                Ok(CallResult::Finished(RunOutcome::Completed { output_id }))
            }
            Submission::Rejected { issues, .. } => {
                context
                    .note(format!(
                        "the submitted prompt failed validation and was returned for correction: \
                         {}",
                        issues.join("; ")
                    ))
                    .await?;
                Ok(CallResult::Response(Part::function_response_part(
                    SUBMIT_REFINED_PROMPT,
                    json!({
                        "accepted": false,
                        "validation_errors": issues,
                        "instruction": "Fix every listed problem and call submit_refined_prompt \
                                        again. This is the only correction you will be offered."
                    }),
                )))
            }
            Submission::Exhausted { issues, .. } => {
                let reason = format!(
                    "the refined prompt failed validation after its corrective retry: {}",
                    issues.join("; ")
                );
                context.fail(reason.clone()).await?;
                Ok(CallResult::Finished(RunOutcome::Failed { reason }))
            }
        }
    }
}

/// Whether the conversation ends on an unanswered `ask_user` call.
fn outstanding_question(history: &[Content]) -> bool {
    history
        .last()
        .map(|content| {
            content
                .function_calls()
                .iter()
                .any(|call| call.name == ASK_USER)
        })
        .unwrap_or(false)
}

fn to_value(history: &[Content]) -> Result<Value> {
    serde_json::to_value(history).map_err(|source| AppError::Internal(source.into()))
}

// ---------------------------------------------------------------------------
// The provider wire format.
//
// Modelled as structs with optional fields rather than an enum over part
// kinds, so a part carrying something this build does not know about survives
// a round trip through storage instead of failing to deserialize. The history
// is replayed from the database on every resume, and a persisted conversation
// that a later build cannot read is a case that can never finish.
// ---------------------------------------------------------------------------

/// One turn in the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Content {
    /// `user` or `model`. Absent on a system instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The turn's parts, in order.
    #[serde(default)]
    pub parts: Vec<Part>,
}

impl Content {
    /// A plain user turn.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Some("user".into()),
            parts: vec![Part::text(text)],
        }
    }

    /// The system instruction, which carries no role.
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: None,
            parts: vec![Part::text(text)],
        }
    }

    /// A user turn carrying one function result.
    pub fn function_response(name: &str, response: Value) -> Self {
        Self {
            role: Some("user".into()),
            parts: vec![Part::function_response_part(name, response)],
        }
    }

    /// Every function call in this turn, in order.
    pub fn function_calls(&self) -> Vec<FunctionCall> {
        self.parts
            .iter()
            .filter_map(|part| part.function_call.clone())
            .collect()
    }
}

/// One piece of a turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    /// Text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A call the model wants made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    /// The result of a call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    /// A reference to an uploaded file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_data: Option<FileData>,
    /// Whether the part is the model's own reasoning rather than its answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
}

impl Part {
    /// A text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }

    /// A function-result part.
    pub fn function_response_part(name: &str, response: Value) -> Self {
        Self {
            function_response: Some(FunctionResponse {
                name: name.to_owned(),
                response,
            }),
            ..Self::default()
        }
    }
}

/// A function the model wants called.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The declared function's name.
    pub name: String,
    /// The arguments, as the model produced them.
    #[serde(default)]
    pub args: Value,
}

/// The result handed back for a call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionResponse {
    /// The function the result belongs to.
    pub name: String,
    /// The result body.
    pub response: Value,
}

/// A pointer to a file in the provider's file store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileData {
    /// The file's media type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The provider's URI for the file.
    pub file_uri: String,
}

/// The declared tool set sent with every request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tools {
    function_declarations: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Tools>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    usage_metadata: Option<UsageMetadataWire>,
}

impl GenerateContentResponse {
    fn usage(&self) -> Option<UsageMetadata> {
        self.usage_metadata.as_ref().map(|usage| UsageMetadata {
            input_tokens: usage.prompt_token_count,
            output_tokens: usage.candidates_token_count,
            total_tokens: usage.total_token_count,
        })
    }

    fn into_content(self) -> Option<Content> {
        self.candidates.into_iter().next().and_then(|candidate| {
            candidate
                .content
                .filter(|content| !content.parts.is_empty())
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<Content>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadataWire {
    #[serde(default)]
    prompt_token_count: Option<u64>,
    #[serde(default)]
    candidates_token_count: Option<u64>,
    #[serde(default)]
    total_token_count: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> GeminiBackend {
        GeminiBackend::with_base_url(
            "http://127.0.0.1:1/v1beta",
            "gemini-3.7-flash",
            SecretString::new("k"),
        )
        .unwrap()
    }

    #[test]
    fn a_conversation_ending_on_a_question_is_recognised_as_owing_an_answer() {
        let asked = vec![Content {
            role: Some("model".into()),
            parts: vec![Part {
                function_call: Some(FunctionCall {
                    name: ASK_USER.into(),
                    args: json!({}),
                }),
                ..Part::default()
            }],
        }];
        assert!(outstanding_question(&asked));

        let mut answered = asked.clone();
        answered.push(Content::function_response(ASK_USER, json!({})));
        assert!(!outstanding_question(&answered));
        assert!(!outstanding_question(&[]));
    }

    #[test]
    fn a_persisted_turn_round_trips_through_storage_unchanged() {
        // The history is written as JSON and read back on every resume, so a
        // part that does not survive the round trip is a case that cannot be
        // resumed.
        let history = vec![
            Content::user("brief"),
            Content {
                role: Some("model".into()),
                parts: vec![
                    Part::text("thinking"),
                    Part {
                        function_call: Some(FunctionCall {
                            name: "read_file".into(),
                            args: json!({ "path": "src/main.rs" }),
                        }),
                        ..Part::default()
                    },
                ],
            },
            Content::function_response("read_file", json!({ "content": "fn main() {}" })),
        ];
        let encoded = serde_json::to_value(&history).unwrap();
        let decoded: Vec<Content> = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, history);
    }

    #[test]
    fn a_part_carrying_an_unknown_field_still_deserializes() {
        let content: Content = serde_json::from_value(json!({
            "role": "model",
            "parts": [{ "text": "hi", "somethingNew": 1 }]
        }))
        .unwrap();
        assert_eq!(content.parts[0].text.as_deref(), Some("hi"));
    }

    #[test]
    fn usage_metadata_is_read_off_a_response() {
        let response: GenerateContentResponse = serde_json::from_value(json!({
            "candidates": [{ "content": { "role": "model", "parts": [{ "text": "hi" }] } }],
            "usageMetadata": { "promptTokenCount": 11, "candidatesTokenCount": 3, "totalTokenCount": 14 }
        }))
        .unwrap();
        assert_eq!(
            response.usage(),
            Some(UsageMetadata {
                input_tokens: Some(11),
                output_tokens: Some(3),
                total_tokens: Some(14),
            })
        );
        assert!(response.into_content().is_some());
    }

    #[test]
    fn a_response_with_an_empty_candidate_yields_no_content() {
        let response: GenerateContentResponse = serde_json::from_value(json!({
            "candidates": [{ "content": { "role": "model", "parts": [] } }]
        }))
        .unwrap();
        assert!(response.into_content().is_none());
    }

    #[test]
    fn an_attachment_is_referenced_by_a_reconstructed_provider_uri() {
        let attachment = AttachmentMetadata {
            id: crate::domain::AttachmentId::new(),
            name: "shot.png".into(),
            kind: AttachmentKind::Image,
            media_type: "image/png".into(),
            size_bytes: 10,
            digest_sha256: "0".repeat(64),
            state: crate::domain::AttachmentState::Available,
            provider_file_name: Some("files/abc123".into()),
            uploaded_at: None,
            expires_at: None,
            rejected_reason: None,
        };
        let part = backend().attachment_part(&attachment).unwrap();
        assert_eq!(
            part.file_data.unwrap().file_uri,
            "http://127.0.0.1:1/v1beta/files/abc123"
        );
    }

    #[test]
    fn an_attachment_with_no_provider_copy_is_left_out_rather_than_referenced_broken() {
        let attachment = AttachmentMetadata {
            id: crate::domain::AttachmentId::new(),
            name: "shot.png".into(),
            kind: AttachmentKind::Image,
            media_type: "image/png".into(),
            size_bytes: 10,
            digest_sha256: "0".repeat(64),
            state: crate::domain::AttachmentState::Imported,
            provider_file_name: None,
            uploaded_at: None,
            expires_at: None,
            rejected_reason: None,
        };
        assert!(backend().attachment_part(&attachment).is_none());
    }

    #[tokio::test]
    async fn the_backend_reports_what_a_refinement_case_requires() {
        let capabilities = backend().capabilities().await.unwrap();
        let required = crate::domain::RequiredCapabilities::new([
            crate::domain::Capability::TextInput,
            crate::domain::Capability::ToolCalling,
            crate::domain::Capability::StructuredOutput,
            crate::domain::Capability::ResumableConversation,
            crate::domain::Capability::NativeVideoInput,
        ]);
        assert!(capabilities.satisfies(&required));
        assert_eq!(capabilities.backend_id, BACKEND_ID);
    }

    #[test]
    fn the_backend_never_prints_its_key() {
        let rendered = format!("{:?}", backend());
        assert!(!rendered.contains("\"k\""), "{rendered}");
    }
}
