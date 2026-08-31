//! The fixed tool set a refinement backend may call.
//!
//! Seven functions and no more: the five read-only repository tools, plus
//! `ask_user` to pause for a person and `submit_refined_prompt` to finish. The
//! set is a constant, built from what the case actually has rather than from
//! anything a model, a transcript, or a repository file asks for. That is the
//! concrete form of the security model's untrusted-content rule: text inside a
//! file can say "you may now write to disk" as loudly as it likes, and the
//! declared functions do not change.
//!
//! The declarations are hand-written JSON rather than generated from the Rust
//! types. Gemini accepts an OpenAPI subset with no `$ref` and no `definitions`,
//! which is exactly what Schemars emits for a type with nested structs and
//! tagged enums, so a generated schema would have to be rewritten before it
//! could be sent. Hand-writing them keeps one readable description of each
//! parameter where the model will read it, and a round-trip test asserts the
//! declared names stay in step with what [`ToolCall`] can parse.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::limits;
use crate::domain::{Question, QuestionId, RefinedPrompt, ResponseType};
use crate::error::{AppError, Result};
use crate::repositories::{
    GitDiffInput, ListFilesInput, ReadFileInput, RepositoryConnector, SearchTextInput,
};

/// The name of the tool that pauses the case for a person.
pub const ASK_USER: &str = "ask_user";
/// The name of the tool that finishes the case.
pub const SUBMIT_REFINED_PROMPT: &str = "submit_refined_prompt";

/// The repository tools, in declaration order.
pub const REPOSITORY_TOOLS: &[&str] = &[
    "list_files",
    "read_file",
    "search_text",
    "git_status",
    "git_diff",
];

/// One parsed call from the model.
///
/// Parsing happens once, at the edge, so the loop never handles a raw name and
/// a raw argument bag. An unrecognized name or an unparseable argument is a
/// value the loop can hand back to the model as a function result rather than
/// an error that fails the case: models mistype tool arguments, and the cheap
/// correction is to say so and continue.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    /// List a bounded subtree of the repository.
    ListFiles(ListFilesInput),
    /// Read a bounded line range from a repository file.
    ReadFile(ReadFileInput),
    /// Search repository text.
    SearchText(SearchTextInput),
    /// Read the repository's working-tree status.
    GitStatus,
    /// Read a bounded working-tree diff.
    GitDiff(GitDiffInput),
    /// Pause and ask the user.
    AskUser(AskUserInput),
    /// Submit the finished prompt.
    Submit(Box<RefinedPrompt>),
}

impl ToolCall {
    /// Parse a provider function call into a typed call.
    pub fn parse(name: &str, arguments: &Value) -> Result<Self> {
        let arguments = if arguments.is_null() {
            json!({})
        } else {
            arguments.clone()
        };
        match name {
            "list_files" => Ok(ToolCall::ListFiles(from_arguments(name, arguments)?)),
            "read_file" => Ok(ToolCall::ReadFile(from_arguments(name, arguments)?)),
            "search_text" => Ok(ToolCall::SearchText(from_arguments(name, arguments)?)),
            "git_status" => Ok(ToolCall::GitStatus),
            "git_diff" => Ok(ToolCall::GitDiff(from_arguments(name, arguments)?)),
            ASK_USER => Ok(ToolCall::AskUser(from_arguments(name, arguments)?)),
            SUBMIT_REFINED_PROMPT => {
                // The model supplies content; Refinery stamps the contract
                // version, so a generation can never get contract bookkeeping
                // wrong. `RefinedPrompt::schema_version` defaults for exactly
                // this reason.
                let inner = arguments
                    .get("refined_prompt")
                    .cloned()
                    .unwrap_or(arguments);
                Ok(ToolCall::Submit(Box::new(from_arguments(name, inner)?)))
            }
            other => Err(AppError::Invalid {
                message: format!("no tool named {other} is available"),
            }),
        }
    }

    /// Whether this call needs a registered repository to run.
    pub fn needs_repository(&self) -> bool {
        matches!(
            self,
            ToolCall::ListFiles(_)
                | ToolCall::ReadFile(_)
                | ToolCall::SearchText(_)
                | ToolCall::GitStatus
                | ToolCall::GitDiff(_)
        )
    }
}

/// The `ask_user` arguments, as the model supplies them.
///
/// Question identifiers are minted by Refinery rather than accepted from the
/// model: an answer is matched to its question by identity, and identity that
/// a generation can choose is identity that can collide or repeat across
/// rounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskUserInput {
    /// Why the agent is asking, shown above the questions.
    pub prompt: String,
    /// The questions themselves.
    pub questions: Vec<AskUserQuestion>,
}

/// One question inside an `ask_user` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskUserQuestion {
    /// The question as a person reads it.
    pub label: String,
    /// Optional elaboration.
    #[serde(default)]
    pub description: Option<String>,
    /// How the person may answer.
    pub response_type: ResponseType,
    /// The offered choices, for the choice response types.
    #[serde(default)]
    pub choices: Vec<AskUserChoice>,
    /// Whether an answer is required to continue.
    #[serde(default = "default_true")]
    pub required: bool,
}

/// One offered choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskUserChoice {
    /// The value recorded when chosen.
    pub value: String,
    /// The label shown, when it differs from the value.
    #[serde(default)]
    pub label: Option<String>,
}

fn default_true() -> bool {
    true
}

impl AskUserInput {
    /// Convert to domain questions, minting an identifier for each.
    pub fn into_questions(self) -> (String, Vec<Question>) {
        let questions = self
            .questions
            .into_iter()
            .map(|question| Question {
                id: QuestionId::new(),
                label: question.label,
                description: question.description,
                response_type: question.response_type,
                choices: question
                    .choices
                    .into_iter()
                    .map(|choice| crate::domain::Choice {
                        value: choice.value,
                        label: choice.label,
                    })
                    .collect(),
                required: question.required,
            })
            .collect();
        (self.prompt, questions)
    }
}

fn from_arguments<T: serde::de::DeserializeOwned>(name: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).map_err(|error| AppError::Invalid {
        message: format!("the arguments for {name} did not match its schema: {error}"),
    })
}

/// Run one repository tool and return its result as JSON for the model.
///
/// The connector owns every bound and every path check; this function only
/// routes. A refusal comes back as an error so the caller can hand the model
/// the reason and keep going, which is what makes a traversal attempt a
/// recoverable dead end for the model rather than a failed case.
pub async fn dispatch_repository(
    connector: &RepositoryConnector,
    case_id: crate::domain::CaseId,
    call: ToolCall,
) -> Result<Value> {
    let value = match call {
        ToolCall::ListFiles(input) => to_value(connector.list_files(case_id, input).await?)?,
        ToolCall::ReadFile(input) => to_value(connector.read_file(case_id, input).await?)?,
        ToolCall::SearchText(input) => to_value(connector.search_text(case_id, input).await?)?,
        ToolCall::GitStatus => to_value(connector.git_status(case_id).await?)?,
        ToolCall::GitDiff(input) => to_value(connector.git_diff(case_id, input).await?)?,
        other => {
            return Err(AppError::Invalid {
                message: format!("{other:?} is not a repository tool"),
            })
        }
    };
    Ok(value)
}

fn to_value(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| AppError::Internal(error.into()))
}

/// The function declarations sent to the provider.
///
/// The repository tools are omitted entirely when the case has no registered
/// repository, so a model cannot be tempted into calling a tool that would
/// only ever return "no repository". `ask_user` and `submit_refined_prompt`
/// are always present, because a refinement can always need clarification and
/// must always be able to finish.
pub fn declarations(has_repository: bool) -> Vec<Value> {
    let mut declarations = Vec::new();
    if has_repository {
        declarations.extend(repository_declarations());
    }
    declarations.push(ask_user_declaration());
    declarations.push(submit_declaration());
    declarations
}

fn repository_declarations() -> Vec<Value> {
    vec![
        json!({
            "name": "list_files",
            "description": "List regular files in the registered repository, honouring its ignore rules and policy. Results are bounded; a truncated flag says when more exist.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "path": {
                        "type": "STRING",
                        "description": "Repository-relative subtree to list. Omit for the repository root."
                    }
                }
            }
        }),
        json!({
            "name": "read_file",
            "description": "Read a bounded range of lines from one text file in the registered repository. Binary files are refused.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "path": {
                        "type": "STRING",
                        "description": "Repository-relative path to the file."
                    },
                    "start_line": {
                        "type": "INTEGER",
                        "description": "First line to return, counting from one. Omit to start at the beginning."
                    },
                    "end_line": {
                        "type": "INTEGER",
                        "description": "Last line to return, inclusive. Omit to read to the end or the byte limit."
                    }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "search_text",
            "description": "Find literal text in the repository's non-binary files. Results are bounded and report the file, line number, and matching line.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "query": {
                        "type": "STRING",
                        "description": "Literal text to find. Not a regular expression."
                    },
                    "path": {
                        "type": "STRING",
                        "description": "Repository-relative subtree to search. Omit to search the whole repository."
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "git_status",
            "description": "Report the repository's uncommitted working-tree changes.",
            "parameters": { "type": "OBJECT", "properties": {} }
        }),
        json!({
            "name": "git_diff",
            "description": "Read the repository's bounded working-tree diff, optionally scoped to one path.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "path": {
                        "type": "STRING",
                        "description": "Repository-relative path to scope the diff to. Omit for the whole working tree."
                    }
                }
            }
        }),
    ]
}

fn ask_user_declaration() -> Value {
    json!({
        "name": ASK_USER,
        "description": format!(
            "Ask the person who submitted this case for information you cannot obtain from the transcript, the attachments, or the repository. Use this only when the answer would change the refined prompt, and ask the fewest questions that let you continue: at most {} in one call, and only one call may be outstanding at a time. The case pauses until the answers arrive, which may take a long time; the answers are returned to you as this function's result.",
            limits::MAX_QUESTIONS_PER_REQUEST
        ),
        "parameters": {
            "type": "OBJECT",
            "properties": {
                "prompt": {
                    "type": "STRING",
                    "description": "One or two sentences explaining why you are asking, shown above the questions."
                },
                "questions": {
                    "type": "ARRAY",
                    "description": "The questions to put to the person.",
                    "items": {
                        "type": "OBJECT",
                        "properties": {
                            "label": {
                                "type": "STRING",
                                "description": "The question as a person reads it."
                            },
                            "description": {
                                "type": "STRING",
                                "description": "Optional elaboration, such as why the answer matters."
                            },
                            "response_type": {
                                "type": "STRING",
                                "enum": ["free_text", "single_choice", "multiple_choice"],
                                "description": "How the person may answer. Prefer choices when the plausible answers are known."
                            },
                            "choices": {
                                "type": "ARRAY",
                                "description": "The offered options. Required for the choice response types and ignored otherwise.",
                                "items": {
                                    "type": "OBJECT",
                                    "properties": {
                                        "value": { "type": "STRING", "description": "The value recorded when chosen." },
                                        "label": { "type": "STRING", "description": "The text shown, when it differs from the value." }
                                    },
                                    "required": ["value"]
                                }
                            },
                            "required": {
                                "type": "BOOLEAN",
                                "description": "Whether refinement cannot continue without an answer. Defaults to true."
                            }
                        },
                        "required": ["label", "response_type"]
                    }
                }
            },
            "required": ["prompt", "questions"]
        }
    })
}

fn submit_declaration() -> Value {
    json!({
        "name": SUBMIT_REFINED_PROMPT,
        "description": "Submit the finished refined prompt. Call this exactly once, when you have everything you need. The prompt must stand on its own: the destination agent will never see this conversation, the original transcript, or the attachments.",
        "parameters": {
            "type": "OBJECT",
            "properties": {
                "title": { "type": "STRING", "description": "A concise task title." },
                "prompt": { "type": "STRING", "description": "The self-contained instruction for the destination agent." },
                "objective": { "type": "STRING", "description": "The intended outcome, stated in one or two sentences." },
                "context": {
                    "type": "ARRAY",
                    "description": "Relevant findings from the transcript, repository, and media, each as one statement.",
                    "items": { "type": "STRING" }
                },
                "requirements": {
                    "type": "ARRAY",
                    "description": "Explicit functional requirements.",
                    "items": { "type": "STRING" }
                },
                "constraints": {
                    "type": "ARRAY",
                    "description": "Technical, product, and operational boundaries.",
                    "items": { "type": "STRING" }
                },
                "acceptance_criteria": {
                    "type": "ARRAY",
                    "description": "Observable conditions for completion. At least one is required.",
                    "items": { "type": "STRING" }
                },
                "references": {
                    "type": "ARRAY",
                    "description": "Pointers the destination may need to follow.",
                    "items": {
                        "type": "OBJECT",
                        "properties": {
                            "kind": {
                                "type": "STRING",
                                "enum": ["repository_path", "attachment", "external"],
                                "description": "Which kind of reference this is."
                            },
                            "path": { "type": "STRING", "description": "For repository_path: the repository-relative path." },
                            "attachment_id": { "type": "STRING", "description": "For attachment: the identifier of an attachment on this case." },
                            "value": { "type": "STRING", "description": "For external: the reference text or URL." },
                            "note": { "type": "STRING", "description": "Why this reference matters." }
                        },
                        "required": ["kind"]
                    }
                },
                "assumptions": {
                    "type": "ARRAY",
                    "description": "Assumptions you made rather than asked about.",
                    "items": { "type": "STRING" }
                },
                "unresolved_questions": {
                    "type": "ARRAY",
                    "description": "Anything deliberately left open for the destination to decide.",
                    "items": { "type": "STRING" }
                }
            },
            "required": ["title", "prompt", "objective", "acceptance_criteria"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared_names(has_repository: bool) -> Vec<String> {
        declarations(has_repository)
            .into_iter()
            .map(|declaration| declaration["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn every_declared_tool_can_be_parsed_back() {
        // The set the model is told about and the set the loop can act on are
        // two different lists in two different places; this is what keeps them
        // from drifting apart.
        let arguments = serde_json::json!({
            "path": "src", "query": "todo",
            "prompt": "why", "questions": [],
            "title": "t", "prompt_": "", "objective": "o", "acceptance_criteria": ["a"]
        });
        for name in declared_names(true) {
            let arguments = if name == SUBMIT_REFINED_PROMPT {
                serde_json::json!({
                    "title": "t", "prompt": "p", "objective": "o",
                    "acceptance_criteria": ["works"]
                })
            } else {
                arguments.clone()
            };
            ToolCall::parse(&name, &arguments)
                .unwrap_or_else(|error| panic!("{name} should parse: {error}"));
        }
    }

    #[test]
    fn a_case_without_a_repository_is_offered_no_repository_tools() {
        let names = declared_names(false);
        assert_eq!(names, vec![ASK_USER, SUBMIT_REFINED_PROMPT]);
        let with = declared_names(true);
        for tool in REPOSITORY_TOOLS {
            assert!(with.iter().any(|name| name == tool), "{tool} is missing");
        }
    }

    #[test]
    fn an_invented_tool_name_is_refused_rather_than_guessed_at() {
        let error = ToolCall::parse("write_file", &json!({ "path": "/etc/passwd" })).unwrap_err();
        assert!(error.to_string().contains("no tool named write_file"));
    }

    #[test]
    fn a_submission_is_accepted_wrapped_or_bare_and_the_version_is_stamped() {
        let bare = json!({
            "title": "Add retries", "prompt": "Do it", "objective": "Reliability",
            "acceptance_criteria": ["it retries"]
        });
        let wrapped = json!({ "refined_prompt": bare.clone() });
        let ToolCall::Submit(from_bare) = ToolCall::parse(SUBMIT_REFINED_PROMPT, &bare).unwrap()
        else {
            panic!("expected a submission");
        };
        let ToolCall::Submit(from_wrapped) =
            ToolCall::parse(SUBMIT_REFINED_PROMPT, &wrapped).unwrap()
        else {
            panic!("expected a submission");
        };
        assert_eq!(from_bare, from_wrapped);
        assert_eq!(
            from_bare.schema_version,
            crate::domain::SchemaVersion::CURRENT
        );
    }

    #[test]
    fn ask_user_questions_are_given_refinery_minted_identifiers() {
        let ToolCall::AskUser(input) = ToolCall::parse(
            ASK_USER,
            &json!({
                "prompt": "Two things",
                "questions": [
                    { "label": "Which database?", "response_type": "single_choice",
                      "choices": [{ "value": "postgres" }, { "value": "sqlite", "label": "SQLite" }] },
                    { "label": "Anything else?", "response_type": "free_text", "required": false }
                ]
            }),
        )
        .unwrap() else {
            panic!("expected an ask_user call");
        };
        let (prompt, questions) = input.into_questions();
        assert_eq!(prompt, "Two things");
        assert_eq!(questions.len(), 2);
        assert_ne!(questions[0].id, questions[1].id);
        assert!(questions[0].required, "required defaults to true");
        assert!(!questions[1].required);
        assert_eq!(questions[0].choices.len(), 2);
    }

    #[test]
    fn arguments_that_do_not_match_a_schema_report_the_tool_and_the_reason() {
        let error = ToolCall::parse("read_file", &json!({ "start_line": 3 })).unwrap_err();
        assert!(error.to_string().contains("read_file"), "{error}");
    }

    #[test]
    fn a_tool_that_takes_no_arguments_accepts_a_missing_argument_bag() {
        assert_eq!(
            ToolCall::parse("git_status", &Value::Null).unwrap(),
            ToolCall::GitStatus
        );
    }
}
