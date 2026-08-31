//! Questions and answers: the durable clarification loop.
//!
//! A backend's `ask_user` call persists a question request, moves the case to
//! `awaiting_answer`, and completes the current job. An arriving answer
//! validates against the question definition and enqueues a resume job. At
//! most one outstanding question request exists per case, enforced in storage.
//!
//! The reason this is a service rather than two storage calls is the fourth
//! step of the product's sequence: *wait without holding an HTTP request or
//! process lock*. Nothing here blocks. Asking is a write that ends the current
//! unit of work, and answering is a separate write that schedules the next
//! one, so the gap between them can be a second or a week with a restart in
//! the middle and neither side has to care.

use chrono::Utc;

use crate::domain::{
    AnswerSet, CaseId, Question, QuestionRequest, QuestionRequestId, QuestionRequestStatus,
    SchemaVersion,
};
use crate::error::{AppError, Result};
use crate::storage::Storage;

/// The clarification loop, bound to one durable store.
#[derive(Clone, Debug)]
pub struct QuestionService {
    storage: Storage,
}

impl QuestionService {
    /// Bind the service to the store that owns the question rows.
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }

    /// Raise a question request against a running case.
    ///
    /// The one-outstanding-request rule is checked here and enforced by a
    /// unique partial index underneath. The check exists to turn a race that
    /// would surface as a constraint violation into the sentence a backend can
    /// act on, and the index exists because the check alone would not survive
    /// two workers reaching this line at once.
    pub async fn ask(
        &self,
        case_id: CaseId,
        prompt: impl Into<String>,
        questions: Vec<Question>,
    ) -> Result<QuestionRequest> {
        if let Some(existing) = self.storage.pending_question(case_id).await? {
            return Err(AppError::Invalid {
                message: format!(
                    "case {case_id} is already waiting on question request {}",
                    existing.id
                ),
            });
        }
        let request = QuestionRequest {
            schema_version: SchemaVersion::CURRENT,
            id: QuestionRequestId::new(),
            case_id,
            prompt: prompt.into(),
            questions,
            created_at: Utc::now(),
            status: QuestionRequestStatus::Pending,
        };
        self.storage.record_question(&request).await?;
        Ok(request)
    }

    /// The case's outstanding request, when it has one.
    pub async fn pending(&self, case_id: CaseId) -> Result<Option<QuestionRequest>> {
        self.storage.pending_question(case_id).await
    }

    /// Validate and store answers, which schedules the case's resume.
    pub async fn answer(&self, answers: &AnswerSet) -> Result<()> {
        self.storage.apply_answer(answers).await
    }

    /// The most recently answered request and its answers, for a resuming
    /// backend that has to hand a function result back to the model.
    pub async fn latest_answers(&self, case_id: CaseId) -> Result<Option<AnswerSet>> {
        self.storage.latest_answers(case_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Answer, AnswerValue, QuestionId, RefinementRequest, ResponseType};
    use crate::storage::CreateCaseResult;

    async fn running_case() -> (Storage, CaseId) {
        let path = tempfile::tempdir().unwrap().keep().join("questions.db");
        let storage = Storage::open(path).await.unwrap();
        let request: RefinementRequest = serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/refinement_request.json"
        ))
        .unwrap();
        let CreateCaseResult::Created(case_id) =
            storage.create_case_idempotent(&request).await.unwrap()
        else {
            panic!("a fresh store creates the case");
        };
        storage.mark_preparation_started(case_id).await.unwrap();
        storage.mark_backend_started(case_id).await.unwrap();
        (storage, case_id)
    }

    fn question() -> Question {
        Question {
            id: QuestionId::new(),
            label: "Which database?".into(),
            description: None,
            response_type: ResponseType::FreeText,
            choices: Vec::new(),
            required: true,
        }
    }

    #[tokio::test]
    async fn a_second_question_is_refused_while_one_is_outstanding() {
        let (storage, case_id) = running_case().await;
        let service = QuestionService::new(storage);
        service
            .ask(case_id, "One thing first", vec![question()])
            .await
            .unwrap();
        let error = service
            .ask(case_id, "And another", vec![question()])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already waiting"), "{error}");
    }

    #[tokio::test]
    async fn answering_clears_the_outstanding_request_and_returns_it_for_replay() {
        let (storage, case_id) = running_case().await;
        let service = QuestionService::new(storage);
        let asked = service
            .ask(case_id, "Which database?", vec![question()])
            .await
            .unwrap();
        service
            .answer(&AnswerSet {
                schema_version: SchemaVersion::CURRENT,
                case_id,
                question_request_id: asked.id,
                answers: vec![Answer {
                    question_id: asked.questions[0].id,
                    value: AnswerValue::Text {
                        text: "Postgres".into(),
                    },
                    answered_at: Utc::now(),
                    answered_by: "user".into(),
                }],
            })
            .await
            .unwrap();
        assert!(service.pending(case_id).await.unwrap().is_none());
        let replayed = service.latest_answers(case_id).await.unwrap().unwrap();
        assert_eq!(replayed.question_request_id, asked.id);
        assert_eq!(replayed.answers.len(), 1);
    }
}
