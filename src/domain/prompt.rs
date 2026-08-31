//! The output contract: the refined prompt, and the deterministic checks that
//! decide whether it is fit to deliver.
//!
//! The product promise is that the `prompt` field stands on its own — a
//! destination agent should never need Refinery's internal conversation to
//! understand the work. Most of that is a judgement the agent makes, but four
//! parts of it are mechanically checkable and are checked here: the object is
//! schema-valid and within its bounds, it states at least one observable
//! acceptance criterion, no required question is left unresolved, and it
//! carries no provider or transport markup that leaked out of the model
//! conversation. Repository-path validity is checked in the repository
//! connector, where a repository handle exists; this module stays pure.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::ids::{AttachmentId, QuestionId};
use crate::domain::limits;
use crate::domain::validation::{check_string_list, check_text, ValidationCode, ValidationReport};
use crate::domain::SchemaVersion;

/// Markers that mean provider or transport framing escaped into the output.
///
/// Each entry is unambiguous framing, never ordinary prose or code a prompt
/// might legitimately discuss. That is the bar for adding one: a false
/// positive fails a good refinement, so a marker that could plausibly appear
/// inside a real instruction does not belong here.
pub const PROVIDER_MARKUP_MARKERS: &[&str] = &[
    "<|im_start|>",
    "<|im_end|>",
    "<|eot_id|>",
    "<|start_header_id|>",
    "<|end_header_id|>",
    "<start_of_turn>",
    "<end_of_turn>",
    "```tool_code",
    "```tool_outputs",
    "<function_call>",
    "</function_call>",
    "<<SYS>>",
    "[/INST]",
];

/// A pointer the destination may need to follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PromptReference {
    /// A path inside the case's registered repository, relative to its root.
    RepositoryPath {
        /// The repository-relative path.
        path: String,
        /// Why it matters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// An attachment imported into the case.
    Attachment {
        /// The attachment.
        attachment_id: AttachmentId,
        /// Why it matters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Anything else worth naming, such as a ticket.
    External {
        /// The reference text or URL.
        value: String,
        /// Why it matters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl PromptReference {
    fn validate_into(&self, report: &mut ValidationReport, field: &str) {
        match self {
            PromptReference::RepositoryPath { path, note } => {
                check_text(
                    report,
                    &format!("{field}.path"),
                    path,
                    limits::MAX_REFERENCE_CHARS,
                    true,
                );
                if std::path::Path::new(path).is_absolute() || path.contains("..") {
                    report.add(
                        format!("{field}.path"),
                        ValidationCode::NotAllowed,
                        "must be a relative path inside the repository, with no parent segments",
                    );
                }
                check_note(report, field, note.as_deref());
            }
            PromptReference::Attachment { note, .. } => check_note(report, field, note.as_deref()),
            PromptReference::External { value, note } => {
                check_text(
                    report,
                    &format!("{field}.value"),
                    value,
                    limits::MAX_REFERENCE_CHARS,
                    true,
                );
                check_note(report, field, note.as_deref());
            }
        }
    }
}

fn check_note(report: &mut ValidationReport, field: &str, note: Option<&str>) {
    if let Some(note) = note {
        check_text(
            report,
            &format!("{field}.note"),
            note,
            limits::MAX_REFERENCE_NOTE_CHARS,
            true,
        );
    }
}

/// The agent's output: a self-contained instruction for a destination agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefinedPrompt {
    /// The contract version.
    ///
    /// Unlike the source-supplied contracts, this defaults: the model produces
    /// the content and Refinery stamps the version, so contract bookkeeping is
    /// never something a generation has to get right.
    #[serde(default)]
    pub schema_version: SchemaVersion,
    /// A concise task title.
    pub title: String,
    /// The self-contained instruction. This is what a destination executes.
    pub prompt: String,
    /// The intended outcome, in one or two sentences.
    pub objective: String,
    /// Relevant findings from the transcript, repository, and media.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
    /// Explicit functional requirements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<String>,
    /// Technical, product, and operational boundaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    /// Observable conditions for completion. Never empty.
    pub acceptance_criteria: Vec<String>,
    /// Local paths and attachments the destination may need.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<PromptReference>,
    /// Assumptions made during refinement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
    /// Anything deliberately left open, stated so the destination is not
    /// surprised by it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_questions: Vec<String>,
}

/// What validation needs to know that the prompt itself cannot say.
#[derive(Debug, Clone, Default)]
pub struct PromptValidationContext {
    /// Required questions the case has raised and not had answered. A prompt
    /// cannot be accepted while any remain, which is what keeps `validating`
    /// from reaching `ready` with an open required question.
    pub unresolved_required_questions: Vec<QuestionId>,
    /// Attachments imported into the case. An attachment reference naming
    /// anything else is a dangling pointer for the destination.
    pub known_attachments: Vec<AttachmentId>,
}

impl RefinedPrompt {
    /// Run every deterministic check against the prompt.
    ///
    /// Pure: no clock, no filesystem, no network, so the agent loop can call it
    /// on a candidate submission and hand the issues straight back to the model
    /// as its one corrective retry.
    pub fn validate(&self, context: &PromptValidationContext) -> Result<(), ValidationReport> {
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
            "title",
            &self.title,
            limits::MAX_TITLE_CHARS,
            true,
        );
        check_text(
            &mut report,
            "prompt",
            &self.prompt,
            limits::MAX_PROMPT_CHARS,
            true,
        );
        check_text(
            &mut report,
            "objective",
            &self.objective,
            limits::MAX_OBJECTIVE_CHARS,
            true,
        );

        for (field, values) in [
            ("context", &self.context),
            ("requirements", &self.requirements),
            ("constraints", &self.constraints),
            ("assumptions", &self.assumptions),
            ("unresolved_questions", &self.unresolved_questions),
        ] {
            check_string_list(
                &mut report,
                field,
                values,
                limits::MAX_LIST_ITEMS,
                limits::MAX_LIST_ITEM_CHARS,
            );
        }

        if self.acceptance_criteria.len() < limits::MIN_ACCEPTANCE_CRITERIA {
            report.add(
                "acceptance_criteria",
                ValidationCode::TooFew,
                format!(
                    "state at least {} observable condition for completion",
                    limits::MIN_ACCEPTANCE_CRITERIA
                ),
            );
        }
        check_string_list(
            &mut report,
            "acceptance_criteria",
            &self.acceptance_criteria,
            limits::MAX_LIST_ITEMS,
            limits::MAX_LIST_ITEM_CHARS,
        );

        if self.references.len() > limits::MAX_REFERENCES {
            report.add(
                "references",
                ValidationCode::TooMany,
                format!(
                    "must have at most {} references, found {}",
                    limits::MAX_REFERENCES,
                    self.references.len()
                ),
            );
        }
        for (index, reference) in self.references.iter().enumerate() {
            let field = format!("references[{index}]");
            reference.validate_into(&mut report, &field);
            if let PromptReference::Attachment { attachment_id, .. } = reference {
                if !context.known_attachments.contains(attachment_id) {
                    report.add(
                        format!("{field}.attachment_id"),
                        ValidationCode::Unknown,
                        format!("attachment {attachment_id} does not belong to this case"),
                    );
                }
            }
        }

        for question_id in &context.unresolved_required_questions {
            report.add(
                "acceptance_criteria",
                ValidationCode::Unanswered,
                format!(
                    "required question {question_id} is unresolved; a prompt cannot be delivered \
                     while an answer is outstanding"
                ),
            );
        }

        self.check_provider_markup(&mut report);
        self.check_total_size(&mut report);

        report.into_result()
    }

    /// Every text field, in one pass, for whole-object checks.
    fn text_fields(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("title", self.title.as_str()),
            ("prompt", self.prompt.as_str()),
            ("objective", self.objective.as_str()),
        ]
        .into_iter()
        .chain(
            [
                ("context", &self.context),
                ("requirements", &self.requirements),
                ("constraints", &self.constraints),
                ("acceptance_criteria", &self.acceptance_criteria),
                ("assumptions", &self.assumptions),
                ("unresolved_questions", &self.unresolved_questions),
            ]
            .into_iter()
            .flat_map(|(field, values)| values.iter().map(move |value| (field, value.as_str()))),
        )
    }

    fn check_provider_markup(&self, report: &mut ValidationReport) {
        for (field, text) in self.text_fields() {
            for marker in PROVIDER_MARKUP_MARKERS {
                if text.contains(marker) {
                    report.add(
                        field,
                        ValidationCode::ProviderMarkup,
                        format!("contains provider markup {marker}, which must never be delivered"),
                    );
                }
            }
        }
    }

    fn check_total_size(&self, report: &mut ValidationReport) {
        let Ok(encoded) = serde_json::to_vec(self) else {
            report.add(
                "",
                ValidationCode::Malformed,
                "the refined prompt could not be serialized",
            );
            return;
        };
        if encoded.len() > limits::MAX_REFINED_PROMPT_BYTES {
            report.add(
                "",
                ValidationCode::TooLong,
                format!(
                    "must serialize to at most {} bytes, found {}",
                    limits::MAX_REFINED_PROMPT_BYTES,
                    encoded.len()
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> RefinedPrompt {
        RefinedPrompt {
            schema_version: SchemaVersion::CURRENT,
            title: "Make the importer resumable".into(),
            prompt: "Add a durable checkpoint to the importer so an interrupted run resumes \
                     from the last committed batch."
                .into(),
            objective: "An interrupted import resumes without duplicating rows.".into(),
            context: vec!["The importer currently restarts from zero.".into()],
            requirements: vec!["Persist a checkpoint after each committed batch.".into()],
            constraints: vec!["No schema change to the public tables.".into()],
            acceptance_criteria: vec![
                "Killing the importer mid-run and restarting it imports every row exactly once."
                    .into(),
            ],
            references: vec![PromptReference::RepositoryPath {
                path: "src/importer/mod.rs".into(),
                note: Some("the run loop".into()),
            }],
            assumptions: vec![],
            unresolved_questions: vec![],
        }
    }

    #[test]
    fn a_good_prompt_validates() {
        prompt()
            .validate(&PromptValidationContext::default())
            .unwrap();
    }

    #[test]
    fn acceptance_criteria_may_not_be_empty() {
        let mut prompt = prompt();
        prompt.acceptance_criteria.clear();
        let report = prompt
            .validate(&PromptValidationContext::default())
            .unwrap_err();
        assert!(report.has_code(ValidationCode::TooFew));
    }

    #[test]
    fn a_blank_acceptance_criterion_is_refused() {
        let mut prompt = prompt();
        prompt.acceptance_criteria.push("   ".into());
        let report = prompt
            .validate(&PromptValidationContext::default())
            .unwrap_err();
        assert!(report.has_field("acceptance_criteria[1]"));
    }

    #[test]
    fn an_unresolved_required_question_blocks_the_prompt() {
        let context = PromptValidationContext {
            unresolved_required_questions: vec![QuestionId::new()],
            known_attachments: vec![],
        };
        let report = prompt().validate(&context).unwrap_err();
        assert!(report.has_code(ValidationCode::Unanswered));
    }

    #[test]
    fn provider_markup_is_refused_wherever_it_appears() {
        for marker in PROVIDER_MARKUP_MARKERS {
            let mut prompt = prompt();
            prompt.prompt.push_str(marker);
            let report = prompt
                .validate(&PromptValidationContext::default())
                .unwrap_err();
            assert!(
                report.has_code(ValidationCode::ProviderMarkup),
                "marker {marker} was not caught"
            );
        }
    }

    #[test]
    fn provider_markup_is_refused_inside_list_items_too() {
        let mut prompt = prompt();
        prompt
            .acceptance_criteria
            .push("done <|im_end|> really done".into());
        let report = prompt
            .validate(&PromptValidationContext::default())
            .unwrap_err();
        assert!(report.has_code(ValidationCode::ProviderMarkup));
    }

    #[test]
    fn ordinary_prose_about_tools_is_not_mistaken_for_markup() {
        let mut prompt = prompt();
        prompt.prompt =
            "Declare a function call for read_file and assert the tool returns JSON.".into();
        prompt
            .validate(&PromptValidationContext::default())
            .unwrap();
    }

    #[test]
    fn an_oversized_prompt_is_refused_as_a_whole() {
        let mut prompt = prompt();
        let full_list: Vec<String> = (0..limits::MAX_LIST_ITEMS)
            .map(|_| "x".repeat(limits::MAX_LIST_ITEM_CHARS))
            .collect();
        prompt.context = full_list.clone();
        prompt.requirements = full_list.clone();
        prompt.constraints = full_list.clone();
        prompt.assumptions = full_list.clone();
        prompt.acceptance_criteria = full_list;
        let report = prompt
            .validate(&PromptValidationContext::default())
            .unwrap_err();
        assert!(report.has_code(ValidationCode::TooLong));
        assert!(report.issues.iter().any(|issue| issue.field.is_empty()));
    }

    #[test]
    fn a_repository_reference_may_not_escape_the_root() {
        let mut prompt = prompt();
        prompt.references = vec![PromptReference::RepositoryPath {
            path: "../../etc/passwd".into(),
            note: None,
        }];
        let report = prompt
            .validate(&PromptValidationContext::default())
            .unwrap_err();
        assert!(report.has_code(ValidationCode::NotAllowed));
    }

    #[test]
    fn an_attachment_reference_must_belong_to_the_case() {
        let mut prompt = prompt();
        prompt.references = vec![PromptReference::Attachment {
            attachment_id: AttachmentId::new(),
            note: None,
        }];
        let report = prompt
            .validate(&PromptValidationContext::default())
            .unwrap_err();
        assert!(report.has_code(ValidationCode::Unknown));

        let known = AttachmentId::new();
        prompt.references = vec![PromptReference::Attachment {
            attachment_id: known,
            note: None,
        }];
        prompt
            .validate(&PromptValidationContext {
                unresolved_required_questions: vec![],
                known_attachments: vec![known],
            })
            .unwrap();
    }

    #[test]
    fn the_model_need_not_supply_the_schema_version() {
        let json = serde_json::json!({
            "title": "t",
            "prompt": "p",
            "objective": "o",
            "acceptance_criteria": ["it works"],
        });
        let parsed: RefinedPrompt = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.schema_version, SchemaVersion::CURRENT);
    }

    #[test]
    fn an_unknown_field_is_refused_so_a_stray_generation_cannot_smuggle_data() {
        let json = serde_json::json!({
            "title": "t",
            "prompt": "p",
            "objective": "o",
            "acceptance_criteria": ["it works"],
            "thoughts": "internal reasoning",
        });
        assert!(serde_json::from_value::<RefinedPrompt>(json).is_err());
    }
}
