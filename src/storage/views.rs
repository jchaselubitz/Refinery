//! The read-side projections the local interface renders.
//!
//! These are queries, not intents: nothing here mutates a snapshot or appends
//! an event. They exist as a separate module because a view has a different
//! obligation from a write. A write must be minimal and transactional; a view
//! must be *bounded* and *safe to display*, and those two rules are easier to
//! keep honest when they are not interleaved with the intent code.
//!
//! Bounded means every list here has a ceiling the caller cannot raise past a
//! constant. A case that went round the question loop forty times has a long
//! event history, and the browser asking for a case detail must not be handed
//! all of it.
//!
//! Safe to display means the projections carry the submitted request back out.
//! That request may contain a destination bearer token, which is a
//! [`crate::domain::SecretString`] and so serializes as its redaction rather
//! than its value — the type does the work, and [`CaseDetail`] does not have
//! to remember to strip anything.

use chrono::{DateTime, Utc};
use sqlx::Row;

use super::{json_error, storage_error, Storage, StoredAttachment};
use crate::domain::{
    Answer, CaseEvent, CaseId, CaseState, DeliveryId, OutputId, QuestionRequest, RefinedPrompt,
    RefinementRequest, SchemaVersion,
};
use crate::error::{AppError, Result, RetryClass};

/// The largest event history one detail response carries.
pub const MAX_DETAIL_EVENTS: u32 = 200;
/// The largest event batch one stream poll carries.
pub const MAX_STREAM_EVENTS: u32 = 200;
/// The largest number of cases one list response carries.
pub const MAX_LIST_CASES: u32 = 200;

/// One delivery attempt as storage holds it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeliveryRecord {
    /// Durable delivery identity.
    pub id: DeliveryId,
    /// The output this delivery carries.
    pub output_id: Option<OutputId>,
    /// Which kind of destination it addresses.
    pub destination_kind: String,
    /// How many attempts this delivery has made.
    pub attempt: u32,
    /// `in_flight`, `accepted`, or `failed`.
    pub status: String,
    /// The idempotency key of the most recent attempt.
    pub idempotency_key: String,
    /// How the last failure was classified, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_class: Option<String>,
    /// The last failure message, already free of secrets by construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the delivery was first started.
    pub created_at: DateTime<Utc>,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// A candidate or accepted refined prompt as storage holds it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredOutput {
    /// Durable output identity.
    pub id: OutputId,
    /// The prompt itself. This is what the interface's copy action yields.
    pub prompt: RefinedPrompt,
    /// Whether it passed deterministic validation.
    pub valid: bool,
    /// Why it did not, when it did not.
    pub validation_issues: Vec<String>,
    /// When the agent submitted it.
    pub created_at: DateTime<Utc>,
    /// When validation accepted it, when it was accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<DateTime<Utc>>,
}

/// One question request together with whatever has been answered.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuestionThread {
    /// The request, including every question in display order.
    pub request: QuestionRequest,
    /// The answers recorded against it, in arrival order.
    pub answers: Vec<Answer>,
}

/// Everything the case detail view shows, in one response.
///
/// One round trip rather than seven, because the browser refreshes this whole
/// object whenever the case's event stream moves and seven requests per event
/// would be seven chances to render a half-updated case.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaseDetail {
    /// The contract version, so the page can refuse a response it predates.
    pub schema_version: SchemaVersion,
    /// Stable case identity.
    pub id: CaseId,
    /// Current lifecycle state.
    pub state: CaseState,
    /// The source's correlation identifier.
    pub request_id: String,
    /// When Refinery accepted the case.
    pub created_at: DateTime<Utc>,
    /// When its snapshot last changed.
    pub updated_at: DateTime<Utc>,
    /// The submitted request: source, transcript, destination, metadata.
    pub request: RefinementRequest,
    /// Imported attachments and their lifecycle states.
    pub attachments: Vec<AttachmentView>,
    /// Every question request raised, newest last, with its answers.
    pub questions: Vec<QuestionThread>,
    /// The outstanding request, when the case is waiting for a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_question: Option<QuestionRequest>,
    /// The accepted refined prompt, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<StoredOutput>,
    /// Delivery attempts, newest last.
    pub deliveries: Vec<DeliveryRecord>,
    /// The tail of the case's history, bounded by [`MAX_DETAIL_EVENTS`].
    pub events: Vec<CaseEvent>,
    /// How many events the case has in total, so the page can say the history
    /// it is showing is a tail rather than the whole of it.
    pub event_count: u64,
}

/// An attachment as the interface shows it: metadata only, never a path.
///
/// The local media path is deliberately dropped here. It is an operational
/// detail of where Refinery keeps bytes, and putting it in a browser response
/// would publish the shape of the user's data directory to any page that can
/// read this endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AttachmentView {
    /// The verified attachment metadata.
    #[serde(flatten)]
    pub metadata: crate::domain::AttachmentMetadata,
    /// Whether its bytes are held locally.
    pub stored_locally: bool,
}

impl From<StoredAttachment> for AttachmentView {
    fn from(stored: StoredAttachment) -> Self {
        Self {
            stored_locally: stored.media_path.is_some(),
            metadata: stored.metadata,
        }
    }
}

impl Storage {
    /// Assemble the case detail projection.
    pub async fn case_detail(&self, case_id: CaseId) -> Result<CaseDetail> {
        let summary = self.case_summary(case_id).await?;
        let request = self.case_request(case_id).await?;
        let attachments = self
            .case_attachments(case_id)
            .await?
            .into_iter()
            .map(AttachmentView::from)
            .collect();
        let questions = self.case_question_threads(case_id).await?;
        let pending_question = self.pending_question(case_id).await?;
        let output = self.accepted_output(case_id).await?;
        let deliveries = self.case_deliveries(case_id).await?;
        let event_count = self.case_event_count(case_id).await?;
        let events = self.case_events_tail(case_id, MAX_DETAIL_EVENTS).await?;
        Ok(CaseDetail {
            schema_version: SchemaVersion::CURRENT,
            id: summary.id,
            state: summary.state,
            request_id: summary.request_id,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            request,
            attachments,
            questions,
            pending_question,
            output,
            deliveries,
            events,
            event_count,
        })
    }

    /// Every question request raised against a case, with its answers.
    pub async fn case_question_threads(&self, case_id: CaseId) -> Result<Vec<QuestionThread>> {
        let rows = sqlx::query(
            "SELECT request_json,status FROM question_requests WHERE case_id=? \
             ORDER BY created_at, id",
        )
        .bind(case_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        let mut threads = Vec::with_capacity(rows.len());
        for row in rows {
            let request = super::runs::stored_question_request(row)?;
            let answers = self.answers_for(request.id).await?;
            threads.push(QuestionThread { request, answers });
        }
        Ok(threads)
    }

    /// The case's accepted refined prompt, when validation has accepted one.
    pub async fn accepted_output(&self, case_id: CaseId) -> Result<Option<StoredOutput>> {
        let row = sqlx::query(
            "SELECT id, prompt_json, valid, validation_issues_json, created_at, accepted_at \
             FROM outputs WHERE case_id=? AND accepted_at IS NOT NULL \
             ORDER BY accepted_at DESC LIMIT 1",
        )
        .bind(case_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_error)?;
        row.map(stored_output).transpose()
    }

    /// Every delivery attempt recorded for a case, oldest first.
    pub async fn case_deliveries(&self, case_id: CaseId) -> Result<Vec<DeliveryRecord>> {
        let rows = sqlx::query(
            "SELECT id, output_id, destination_kind, attempt, status, idempotency_key, \
             retry_class, error, created_at, updated_at \
             FROM deliveries WHERE case_id=? ORDER BY created_at, id",
        )
        .bind(case_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        rows.into_iter().map(delivery_record).collect()
    }

    /// How many events a case has accumulated.
    pub async fn case_event_count(&self, case_id: CaseId) -> Result<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM case_events WHERE case_id=?")
            .bind(case_id.to_string())
            .fetch_one(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(count.max(0) as u64)
    }

    /// The most recent `limit` events, returned oldest first.
    ///
    /// The tail rather than the head: a person opening a case that has been
    /// running for an hour wants to see what just happened, and the beginning
    /// of the history is the part they can already infer.
    pub async fn case_events_tail(&self, case_id: CaseId, limit: u32) -> Result<Vec<CaseEvent>> {
        let rows = sqlx::query(
            "SELECT payload_json FROM case_events WHERE case_id=? ORDER BY sequence DESC LIMIT ?",
        )
        .bind(case_id.to_string())
        .bind(i64::from(limit.min(MAX_DETAIL_EVENTS)))
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        let mut events = rows
            .into_iter()
            .map(|row| {
                serde_json::from_str::<CaseEvent>(&row.get::<String, _>("payload_json"))
                    .map_err(json_error)
            })
            .collect::<Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    /// Events strictly after a sequence position, oldest first.
    ///
    /// This is the stream's resume primitive. A client that saw sequence 12
    /// asks for everything after 12, so a dropped connection costs it nothing
    /// and a reconnect never replays what it already rendered.
    pub async fn case_events_after(
        &self,
        case_id: CaseId,
        after_sequence: u64,
        limit: u32,
    ) -> Result<Vec<CaseEvent>> {
        let rows = sqlx::query(
            "SELECT payload_json FROM case_events WHERE case_id=? AND sequence>? \
             ORDER BY sequence LIMIT ?",
        )
        .bind(case_id.to_string())
        .bind(after_sequence as i64)
        .bind(i64::from(limit.min(MAX_STREAM_EVENTS)))
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        rows.into_iter()
            .map(|row| {
                serde_json::from_str::<CaseEvent>(&row.get::<String, _>("payload_json"))
                    .map_err(json_error)
            })
            .collect()
    }
}

fn stored_output(row: sqlx::sqlite::SqliteRow) -> Result<StoredOutput> {
    Ok(StoredOutput {
        id: parse_output_id(row.get("id"))?,
        prompt: serde_json::from_str(&row.get::<String, _>("prompt_json")).map_err(json_error)?,
        valid: row.get::<i64, _>("valid") != 0,
        validation_issues: serde_json::from_str(&row.get::<String, _>("validation_issues_json"))
            .map_err(json_error)?,
        created_at: parse_timestamp(row.get("created_at"))?,
        accepted_at: row
            .get::<Option<String>, _>("accepted_at")
            .map(parse_timestamp)
            .transpose()?,
    })
}

fn delivery_record(row: sqlx::sqlite::SqliteRow) -> Result<DeliveryRecord> {
    Ok(DeliveryRecord {
        id: row
            .get::<String, _>("id")
            .parse()
            .map_err(|error: crate::domain::InvalidId| invalid_stored(error.to_string()))?,
        output_id: row
            .get::<Option<String>, _>("output_id")
            .map(parse_output_id)
            .transpose()?,
        destination_kind: row.get("destination_kind"),
        attempt: row.get::<i64, _>("attempt").max(0) as u32,
        status: row.get("status"),
        idempotency_key: row.get("idempotency_key"),
        retry_class: row.get("retry_class"),
        error: row.get("error"),
        created_at: parse_timestamp(row.get("created_at"))?,
        updated_at: parse_timestamp(row.get("updated_at"))?,
    })
}

fn parse_output_id(value: String) -> Result<OutputId> {
    value
        .parse()
        .map_err(|error: crate::domain::InvalidId| invalid_stored(error.to_string()))
}

fn parse_timestamp(value: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| invalid_stored(format!("invalid stored timestamp {value}: {error}")))
}

fn invalid_stored(message: String) -> AppError {
    AppError::Storage {
        message,
        retry: RetryClass::NonRetryable,
    }
}
