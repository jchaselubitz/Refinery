//! Contract-wide size and count limits.
//!
//! Every bound the product promises ("bounded transcript", "bounded prompt
//! size", "result limits") lives here as a named constant rather than as a
//! literal at a call site, so a reviewer can read the whole envelope in one
//! place and so the API, the agent loop, and the storage layer cannot disagree
//! about what "bounded" means. These are contract limits: raising one is a
//! contract change and follows the same schema-version discipline as adding a
//! field.

/// Maximum number of transcript messages accepted in one request.
pub const MAX_TRANSCRIPT_MESSAGES: usize = 500;

/// Maximum characters in a single transcript message.
pub const MAX_MESSAGE_CHARS: usize = 100_000;

/// Maximum characters across an entire transcript.
pub const MAX_TRANSCRIPT_CHARS: usize = 400_000;

/// Maximum characters in the optional task hint.
pub const MAX_TASK_HINT_CHARS: usize = 2_000;

/// Maximum attachments accepted in one request.
pub const MAX_ATTACHMENTS: usize = 20;

/// Maximum bytes for a single attachment, sized for a short screen recording.
pub const MAX_ATTACHMENT_BYTES: u64 = 100 * 1024 * 1024;

/// Maximum characters in an attachment display name.
pub const MAX_ATTACHMENT_NAME_CHARS: usize = 255;

/// Maximum entries in the source-supplied correlation metadata map.
pub const MAX_METADATA_ENTRIES: usize = 32;

/// Maximum characters in a metadata key.
pub const MAX_METADATA_KEY_CHARS: usize = 64;

/// Maximum characters in a metadata value.
pub const MAX_METADATA_VALUE_CHARS: usize = 1_024;

/// Maximum characters in a source-supplied identifier such as `request_id` or
/// `idempotency_key`.
pub const MAX_IDENTIFIER_CHARS: usize = 200;

/// Maximum questions in one question request. The agent may group a small set
/// of related questions but is expected to ask the minimum needed to continue.
pub const MAX_QUESTIONS_PER_REQUEST: usize = 5;

/// Maximum characters in the prompt that introduces a question request.
pub const MAX_QUESTION_PROMPT_CHARS: usize = 4_000;

/// Maximum characters in a question label.
pub const MAX_QUESTION_LABEL_CHARS: usize = 200;

/// Maximum characters in a question description.
pub const MAX_QUESTION_DESCRIPTION_CHARS: usize = 2_000;

/// Maximum choices offered for a single choice question.
pub const MAX_QUESTION_CHOICES: usize = 12;

/// Maximum characters in a choice value or label.
pub const MAX_CHOICE_CHARS: usize = 200;

/// Maximum characters in a free-text answer.
pub const MAX_ANSWER_TEXT_CHARS: usize = 10_000;

/// Maximum characters in the refined prompt title.
pub const MAX_TITLE_CHARS: usize = 200;

/// Maximum characters in the self-contained refined prompt body.
pub const MAX_PROMPT_CHARS: usize = 40_000;

/// Maximum characters in the objective statement.
pub const MAX_OBJECTIVE_CHARS: usize = 4_000;

/// Maximum items in any refined-prompt list section.
pub const MAX_LIST_ITEMS: usize = 50;

/// Maximum characters in one refined-prompt list item.
pub const MAX_LIST_ITEM_CHARS: usize = 2_000;

/// Minimum acceptance criteria a refined prompt must carry. A prompt with no
/// observable completion condition is not refined, so this is a hard floor
/// rather than a style preference.
pub const MIN_ACCEPTANCE_CRITERIA: usize = 1;

/// Maximum references (repository paths, attachments, external links).
pub const MAX_REFERENCES: usize = 100;

/// Maximum characters in a reference value such as a repository path.
pub const MAX_REFERENCE_CHARS: usize = 1_024;

/// Maximum characters in a reference note.
pub const MAX_REFERENCE_NOTE_CHARS: usize = 500;

/// Maximum serialized bytes for a complete refined prompt. The per-field
/// limits bound each part; this bounds the whole, so a prompt that is legal
/// field by field still cannot exceed what a destination will accept.
pub const MAX_REFINED_PROMPT_BYTES: usize = 200_000;

/// Maximum case events returned by one bounded event read.
pub const MAX_EVENTS_PER_READ: usize = 500;

/// Maximum bytes returned by a single repository tool invocation.
pub const MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;
