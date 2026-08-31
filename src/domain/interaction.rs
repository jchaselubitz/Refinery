//! Questions and answers: Refinery's own interaction contract.
//!
//! These types are deliberately Refinery's, not any provider's. Gemini sees
//! them as the `ask_user` function schema; Overlord sees them as a question
//! notification; the local interface renders them as a form. All three agree
//! because the definition lives here, and answers are validated against the
//! question that produced them rather than against whatever the provider
//! happens to accept.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::ids::{CaseId, QuestionId, QuestionRequestId};
use crate::domain::limits;
use crate::domain::validation::{check_text, ValidationCode, ValidationReport};
use crate::domain::SchemaVersion;

/// The shape of answer a question accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseType {
    /// Any text.
    FreeText,
    /// Exactly one of the declared choices.
    SingleChoice,
    /// Any subset of the declared choices.
    MultipleChoice,
}

/// One offered answer for a choice question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    /// The stable value recorded when this choice is selected.
    pub value: String,
    /// What the user sees. Defaults to the value when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One question put to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// Refinery's identifier for this question.
    pub id: QuestionId,
    /// The short question text.
    pub label: String,
    /// Why the answer matters, when a sentence of context helps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The shape of answer accepted.
    pub response_type: ResponseType,
    /// The choices offered, for choice questions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    /// Whether refinement can complete without an answer. A case cannot reach
    /// `ready` while a required question is unresolved.
    pub required: bool,
}

impl Question {
    /// Whether this question offers choices.
    pub fn is_choice(&self) -> bool {
        matches!(
            self.response_type,
            ResponseType::SingleChoice | ResponseType::MultipleChoice
        )
    }

    fn validate_into(&self, report: &mut ValidationReport, field: &str) {
        check_text(
            report,
            &format!("{field}.label"),
            &self.label,
            limits::MAX_QUESTION_LABEL_CHARS,
            true,
        );
        if let Some(description) = &self.description {
            check_text(
                report,
                &format!("{field}.description"),
                description,
                limits::MAX_QUESTION_DESCRIPTION_CHARS,
                true,
            );
        }
        if self.is_choice() {
            if self.choices.is_empty() {
                report.add(
                    format!("{field}.choices"),
                    ValidationCode::Empty,
                    "a choice question must offer at least one choice",
                );
            }
            if self.choices.len() > limits::MAX_QUESTION_CHOICES {
                report.add(
                    format!("{field}.choices"),
                    ValidationCode::TooMany,
                    format!(
                        "must offer at most {} choices, found {}",
                        limits::MAX_QUESTION_CHOICES,
                        self.choices.len()
                    ),
                );
            }
            let mut seen: Vec<&str> = Vec::new();
            for (index, choice) in self.choices.iter().enumerate() {
                let choice_field = format!("{field}.choices[{index}]");
                check_text(
                    report,
                    &format!("{choice_field}.value"),
                    &choice.value,
                    limits::MAX_CHOICE_CHARS,
                    true,
                );
                if seen.contains(&choice.value.as_str()) {
                    report.add(
                        format!("{choice_field}.value"),
                        ValidationCode::Duplicate,
                        "choice values must be unique within a question",
                    );
                }
                seen.push(&choice.value);
            }
        } else if !self.choices.is_empty() {
            report.add(
                format!("{field}.choices"),
                ValidationCode::NotAllowed,
                "a free-text question must not offer choices",
            );
        }
    }
}

/// Where a question request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionRequestStatus {
    /// Waiting for the user.
    Pending,
    /// Answered; the case has resumed or is resuming.
    Answered,
    /// Withdrawn because the case was cancelled or failed.
    Cancelled,
}

/// A group of related questions the agent needs answered to continue.
///
/// At most one request is outstanding per case, enforced in storage. A request
/// may carry a small related set, but the agent is expected to ask the minimum
/// needed to make progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionRequest {
    /// The contract version.
    pub schema_version: SchemaVersion,
    /// Refinery's identifier for this request.
    pub id: QuestionRequestId,
    /// The case that needs the answers.
    pub case_id: CaseId,
    /// A sentence framing why the agent is asking.
    pub prompt: String,
    /// The questions, in display order.
    pub questions: Vec<Question>,
    /// When the request was raised, in UTC.
    pub created_at: DateTime<Utc>,
    /// Where the request stands.
    pub status: QuestionRequestStatus,
}

impl QuestionRequest {
    /// Check the request against the interaction contract.
    ///
    /// This runs on the agent's `ask_user` call before the case moves to
    /// `awaiting_answer`, so a malformed question never reaches a person.
    pub fn validate(&self) -> Result<(), ValidationReport> {
        let mut report = ValidationReport::new();

        if !self.schema_version.is_supported() {
            report.add(
                "schema_version",
                ValidationCode::UnsupportedSchemaVersion,
                format!("unsupported schema version {}", self.schema_version),
            );
        }
        check_text(
            &mut report,
            "prompt",
            &self.prompt,
            limits::MAX_QUESTION_PROMPT_CHARS,
            true,
        );
        if self.questions.is_empty() {
            report.add(
                "questions",
                ValidationCode::Empty,
                "a question request must contain at least one question",
            );
        }
        if self.questions.len() > limits::MAX_QUESTIONS_PER_REQUEST {
            report.add(
                "questions",
                ValidationCode::TooMany,
                format!(
                    "must contain at most {} questions, found {}",
                    limits::MAX_QUESTIONS_PER_REQUEST,
                    self.questions.len()
                ),
            );
        }
        let mut seen: Vec<QuestionId> = Vec::new();
        for (index, question) in self.questions.iter().enumerate() {
            let field = format!("questions[{index}]");
            question.validate_into(&mut report, &field);
            if seen.contains(&question.id) {
                report.add(
                    format!("{field}.id"),
                    ValidationCode::Duplicate,
                    "question identifiers must be unique within a request",
                );
            }
            seen.push(question.id);
        }

        report.into_result()
    }

    /// The question with the given identifier.
    pub fn question(&self, id: QuestionId) -> Option<&Question> {
        self.questions.iter().find(|question| question.id == id)
    }

    /// The required questions this answer set leaves unanswered.
    ///
    /// The refined-prompt validator consumes this: a case cannot reach `ready`
    /// while a required question is unresolved.
    pub fn unresolved_required(&self, answers: &AnswerSet) -> Vec<QuestionId> {
        self.questions
            .iter()
            .filter(|question| question.required)
            .filter(|question| answers.answer_for(question.id).is_none())
            .map(|question| question.id)
            .collect()
    }

    /// Check an answer set against these questions.
    ///
    /// Every required question must be answered, every answer must name a
    /// question in this request, and every value must match the shape and the
    /// declared choices of its question.
    pub fn validate_answers(&self, answers: &AnswerSet) -> Result<(), ValidationReport> {
        let mut report = ValidationReport::new();

        if !answers.schema_version.is_supported() {
            report.add(
                "schema_version",
                ValidationCode::UnsupportedSchemaVersion,
                format!("unsupported schema version {}", answers.schema_version),
            );
        }
        if answers.question_request_id != self.id {
            report.add(
                "question_request_id",
                ValidationCode::Unknown,
                "answers name a different question request than the outstanding one",
            );
        }

        let mut seen: Vec<QuestionId> = Vec::new();
        for (index, answer) in answers.answers.iter().enumerate() {
            let field = format!("answers[{index}]");
            if seen.contains(&answer.question_id) {
                report.add(
                    format!("{field}.question_id"),
                    ValidationCode::Duplicate,
                    "a question may be answered only once",
                );
            }
            seen.push(answer.question_id);

            let Some(question) = self.question(answer.question_id) else {
                report.add(
                    format!("{field}.question_id"),
                    ValidationCode::Unknown,
                    format!("no question {} in this request", answer.question_id),
                );
                continue;
            };
            validate_answer_value(&mut report, &field, question, &answer.value);
        }

        for id in self.unresolved_required(answers) {
            report.add(
                "answers",
                ValidationCode::Unanswered,
                format!("question {id} is required and has no answer"),
            );
        }

        report.into_result()
    }
}

fn validate_answer_value(
    report: &mut ValidationReport,
    field: &str,
    question: &Question,
    value: &AnswerValue,
) {
    let allowed = |selected: &str| question.choices.iter().any(|c| c.value == selected);
    match (question.response_type, value) {
        (ResponseType::FreeText, AnswerValue::Text { text }) => {
            check_text(
                report,
                &format!("{field}.value"),
                text,
                limits::MAX_ANSWER_TEXT_CHARS,
                question.required,
            );
        }
        (ResponseType::SingleChoice, AnswerValue::Choice { value: selected }) => {
            if !allowed(selected) {
                report.add(
                    format!("{field}.value"),
                    ValidationCode::NotAllowed,
                    format!("{selected} is not one of the offered choices"),
                );
            }
        }
        (ResponseType::MultipleChoice, AnswerValue::Choices { values: selected }) => {
            if question.required && selected.is_empty() {
                report.add(
                    format!("{field}.value"),
                    ValidationCode::Empty,
                    "select at least one choice",
                );
            }
            if selected.len() > limits::MAX_QUESTION_CHOICES {
                report.add(
                    format!("{field}.value"),
                    ValidationCode::TooMany,
                    "more selections than the question offers",
                );
            }
            let mut seen: Vec<&str> = Vec::new();
            for choice in selected {
                if !allowed(choice) {
                    report.add(
                        format!("{field}.value"),
                        ValidationCode::NotAllowed,
                        format!("{choice} is not one of the offered choices"),
                    );
                }
                if seen.contains(&choice.as_str()) {
                    report.add(
                        format!("{field}.value"),
                        ValidationCode::Duplicate,
                        format!("{choice} was selected twice"),
                    );
                }
                seen.push(choice);
            }
        }
        (expected, actual) => {
            report.add(
                format!("{field}.value"),
                ValidationCode::Malformed,
                format!(
                    "question expects {expected:?} but the answer is {}",
                    actual.kind_str()
                ),
            );
        }
    }
}

/// A validated user response to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    /// The question answered.
    pub question_id: QuestionId,
    /// The response.
    pub value: AnswerValue,
    /// When the answer was given, in UTC.
    pub answered_at: DateTime<Utc>,
    /// Who answered: a person's identifier or the interface that collected it.
    pub answered_by: String,
}

/// A response value, shaped by its question's response type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnswerValue {
    /// Free text.
    Text {
        /// The text.
        text: String,
    },
    /// One selected choice value.
    Choice {
        /// The selected value.
        value: String,
    },
    /// Several selected choice values.
    Choices {
        /// The selected values.
        values: Vec<String>,
    },
}

impl AnswerValue {
    /// The wire spelling of the value shape, for error messages.
    pub fn kind_str(&self) -> &'static str {
        match self {
            AnswerValue::Text { .. } => "text",
            AnswerValue::Choice { .. } => "choice",
            AnswerValue::Choices { .. } => "choices",
        }
    }
}

/// The answers to one question request, submitted together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerSet {
    /// The contract version.
    pub schema_version: SchemaVersion,
    /// The case being answered.
    pub case_id: CaseId,
    /// The outstanding question request these answers respond to.
    pub question_request_id: QuestionRequestId,
    /// The answers, in any order.
    pub answers: Vec<Answer>,
}

impl AnswerSet {
    /// The answer to a given question, when present.
    pub fn answer_for(&self, question_id: QuestionId) -> Option<&Answer> {
        self.answers
            .iter()
            .find(|answer| answer.question_id == question_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_text(required: bool) -> Question {
        Question {
            id: QuestionId::new(),
            label: "Which database backs the importer?".into(),
            description: None,
            response_type: ResponseType::FreeText,
            choices: vec![],
            required,
        }
    }

    fn single_choice() -> Question {
        Question {
            id: QuestionId::new(),
            label: "Target platform?".into(),
            description: Some("Only one is in scope for this milestone.".into()),
            response_type: ResponseType::SingleChoice,
            choices: vec![
                Choice {
                    value: "macos".into(),
                    label: Some("macOS".into()),
                },
                Choice {
                    value: "linux".into(),
                    label: None,
                },
            ],
            required: true,
        }
    }

    fn request(questions: Vec<Question>) -> QuestionRequest {
        QuestionRequest {
            schema_version: SchemaVersion::CURRENT,
            id: QuestionRequestId::new(),
            case_id: CaseId::new(),
            prompt: "Two details decide the approach.".into(),
            questions,
            created_at: Utc::now(),
            status: QuestionRequestStatus::Pending,
        }
    }

    fn answers(request: &QuestionRequest, answers: Vec<Answer>) -> AnswerSet {
        AnswerSet {
            schema_version: SchemaVersion::CURRENT,
            case_id: request.case_id,
            question_request_id: request.id,
            answers,
        }
    }

    fn answer(question_id: QuestionId, value: AnswerValue) -> Answer {
        Answer {
            question_id,
            value,
            answered_at: Utc::now(),
            answered_by: "local_ui".into(),
        }
    }

    #[test]
    fn a_well_formed_request_validates() {
        request(vec![free_text(true), single_choice()])
            .validate()
            .unwrap();
    }

    #[test]
    fn a_choice_question_must_offer_choices() {
        let mut question = single_choice();
        question.choices.clear();
        let report = request(vec![question]).validate().unwrap_err();
        assert!(report.has_field("questions[0].choices"));
    }

    #[test]
    fn a_free_text_question_must_not_offer_choices() {
        let mut question = free_text(true);
        question.choices.push(Choice {
            value: "x".into(),
            label: None,
        });
        let report = request(vec![question]).validate().unwrap_err();
        assert!(report.has_code(ValidationCode::NotAllowed));
    }

    #[test]
    fn too_many_questions_are_refused() {
        let questions = (0..limits::MAX_QUESTIONS_PER_REQUEST + 1)
            .map(|_| free_text(false))
            .collect();
        let report = request(questions).validate().unwrap_err();
        assert!(report.has_code(ValidationCode::TooMany));
    }

    #[test]
    fn matching_answers_validate() {
        let text = free_text(true);
        let choice = single_choice();
        let request = request(vec![text.clone(), choice.clone()]);
        let set = answers(
            &request,
            vec![
                answer(
                    text.id,
                    AnswerValue::Text {
                        text: "postgres".into(),
                    },
                ),
                answer(
                    choice.id,
                    AnswerValue::Choice {
                        value: "macos".into(),
                    },
                ),
            ],
        );
        request.validate_answers(&set).unwrap();
        assert!(request.unresolved_required(&set).is_empty());
    }

    #[test]
    fn a_missing_required_answer_is_reported() {
        let text = free_text(true);
        let request = request(vec![text.clone()]);
        let set = answers(&request, vec![]);
        let report = request.validate_answers(&set).unwrap_err();
        assert!(report.has_code(ValidationCode::Unanswered));
        assert_eq!(request.unresolved_required(&set), vec![text.id]);
    }

    #[test]
    fn an_optional_question_may_go_unanswered() {
        let request = request(vec![free_text(false)]);
        let set = answers(&request, vec![]);
        request.validate_answers(&set).unwrap();
    }

    #[test]
    fn a_choice_outside_the_offered_set_is_refused() {
        let choice = single_choice();
        let request = request(vec![choice.clone()]);
        let set = answers(
            &request,
            vec![answer(
                choice.id,
                AnswerValue::Choice {
                    value: "windows".into(),
                },
            )],
        );
        let report = request.validate_answers(&set).unwrap_err();
        assert!(report.has_code(ValidationCode::NotAllowed));
    }

    #[test]
    fn an_answer_of_the_wrong_shape_is_refused() {
        let choice = single_choice();
        let request = request(vec![choice.clone()]);
        let set = answers(
            &request,
            vec![answer(
                choice.id,
                AnswerValue::Text {
                    text: "macos".into(),
                },
            )],
        );
        let report = request.validate_answers(&set).unwrap_err();
        assert!(report.has_code(ValidationCode::Malformed));
    }

    #[test]
    fn answers_for_another_request_are_refused() {
        let text = free_text(false);
        let request = request(vec![text.clone()]);
        let mut set = answers(&request, vec![]);
        set.question_request_id = QuestionRequestId::new();
        let report = request.validate_answers(&set).unwrap_err();
        assert!(report.has_field("question_request_id"));
    }

    #[test]
    fn an_unknown_question_is_refused() {
        let request = request(vec![free_text(false)]);
        let set = answers(
            &request,
            vec![answer(
                QuestionId::new(),
                AnswerValue::Text { text: "hi".into() },
            )],
        );
        let report = request.validate_answers(&set).unwrap_err();
        assert!(report.has_code(ValidationCode::Unknown));
    }

    #[test]
    fn answering_the_same_question_twice_is_refused() {
        let text = free_text(false);
        let request = request(vec![text.clone()]);
        let set = answers(
            &request,
            vec![
                answer(text.id, AnswerValue::Text { text: "a".into() }),
                answer(text.id, AnswerValue::Text { text: "b".into() }),
            ],
        );
        let report = request.validate_answers(&set).unwrap_err();
        assert!(report.has_code(ValidationCode::Duplicate));
    }

    #[test]
    fn multiple_choice_rejects_repeats_and_unknown_values() {
        let mut question = single_choice();
        question.response_type = ResponseType::MultipleChoice;
        let request = request(vec![question.clone()]);
        let set = answers(
            &request,
            vec![answer(
                question.id,
                AnswerValue::Choices {
                    values: vec!["macos".into(), "macos".into(), "plan9".into()],
                },
            )],
        );
        let report = request.validate_answers(&set).unwrap_err();
        assert!(report.has_code(ValidationCode::Duplicate));
        assert!(report.has_code(ValidationCode::NotAllowed));
    }
}
