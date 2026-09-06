//! Durable SQLite state and intent-level case operations.
//!
//! Every public mutating method owns a short transaction. It updates the case
//! snapshot and appends the corresponding immutable event before committing.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    Row, Sqlite, SqlitePool, Transaction,
};

use crate::{
    cases::transition,
    domain::{
        AnswerSet, AttachmentId, AttachmentMetadata, AttachmentState, CaseEvent, CaseEventPayload,
        CaseId, CaseState, CaseTransition, OutputId, QuestionRequest, RefinementRequest,
    },
    error::{AppError, Result, RetryClass},
};

mod runs;
mod views;
pub use runs::{BackendRun, RunStatus};
pub use views::{
    AttachmentView, CaseDetail, DeliveryRecord, QuestionThread, StoredOutput, MAX_DETAIL_EVENTS,
    MAX_LIST_CASES, MAX_STREAM_EVENTS,
};

/// Embedded, versioned database migrations.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
const DEFAULT_MAX_ATTEMPTS: u32 = 8;

struct QueueSpec<'a> {
    case_id: Option<CaseId>,
    kind: &'a str,
    operation_id: String,
    payload_json: &'a str,
    max_attempts: u32,
}

/// A connection handle configured for one SQLite writer in WAL mode.
#[derive(Clone, Debug)]
pub struct Storage {
    pool: SqlitePool,
}

/// The small, safe-to-display portion of a durable case snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CaseSummary {
    /// Stable case identity.
    pub id: CaseId,
    /// Current lifecycle state.
    pub state: CaseState,
    /// The source correlation identifier.
    pub request_id: String,
    /// When Refinery accepted it.
    pub created_at: DateTime<Utc>,
    /// When its snapshot last changed.
    pub updated_at: DateTime<Utc>,
}

/// Aggregate product signals derived from durable facts.
///
/// These are deliberately queries over the source tables rather than counters
/// updated by call sites. A crash cannot lose a metric increment while keeping
/// the operation it was meant to describe, and an upgraded binary can compute
/// the same definitions over an existing installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductMetrics {
    /// Cases in every lifecycle state, including zeroes.
    pub case_funnel: std::collections::BTreeMap<String, u64>,
    /// Cases accepted through ingress.
    pub submitted_cases: u64,
    /// Cases whose destination accepted the output.
    pub delivered_cases: u64,
    /// Cases that raised at least one question request.
    pub cases_with_questions: u64,
    /// All question requests raised.
    pub question_requests: u64,
    /// Question requests for which valid answers were durably stored.
    pub answered_question_requests: u64,
    /// Answer applications that durably transitioned the case back to running.
    pub resumed_answers: u64,
    /// Logical deliveries started.
    pub deliveries: u64,
    /// HTTP or export attempts across those logical deliveries.
    pub delivery_attempts: u64,
    /// Attempts after the first attempt of a logical delivery.
    pub delivery_retries: u64,
    /// Candidate outputs submitted by backends.
    pub candidate_outputs: u64,
    /// Candidate outputs refused by deterministic validation.
    pub validation_failures: u64,
}

impl ProductMetrics {
    /// A bounded percentage suitable for terminal display.
    pub fn percent(numerator: u64, denominator: u64) -> Option<f64> {
        (denominator != 0).then(|| (numerator as f64 / denominator as f64) * 100.0)
    }
}

/// The idempotent result of accepting an ingress request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateCaseResult {
    /// A new durable case was created and preparation was queued.
    Created(CaseId),
    /// An equal idempotent submission reused the original case.
    Existing(CaseId),
}
impl CreateCaseResult {
    /// The durable case identity.
    pub const fn case_id(self) -> CaseId {
        match self {
            Self::Created(id) | Self::Existing(id) => id,
        }
    }
}

/// An attachment as storage holds it: the versioned metadata plus the local
/// path to its bytes, which is an operational detail rather than part of the
/// published contract and so is not carried on `AttachmentMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    /// The case that owns the attachment and its media directory.
    pub case_id: CaseId,
    /// The verified metadata.
    pub metadata: AttachmentMetadata,
    /// Where the bytes live, absent for a rejected attachment.
    pub media_path: Option<std::path::PathBuf>,
}

/// A lifecycle transition for one attachment.
///
/// Provider fields are applied only when present, so a caller states what
/// changed rather than restating the whole record and risking clearing a
/// field it never meant to touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentStateUpdate {
    /// The state to move to.
    pub state: AttachmentState,
    /// The provider's name for the uploaded copy.
    pub provider_file_name: Option<String>,
    /// When the provider copy was uploaded.
    pub uploaded_at: Option<DateTime<Utc>>,
    /// When the provider copy expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Why the attachment was rejected.
    pub rejected_reason: Option<String>,
    /// Whether to drop any existing provider reference first.
    pub clear_provider: bool,
}

impl Default for AttachmentStateUpdate {
    /// A no-op update that leaves the attachment where it is. Every named
    /// constructor below starts here and changes only what it means to change.
    fn default() -> Self {
        Self {
            state: AttachmentState::Imported,
            provider_file_name: None,
            uploaded_at: None,
            expires_at: None,
            rejected_reason: None,
            clear_provider: false,
        }
    }
}

impl AttachmentStateUpdate {
    /// An upload is starting; forget any previous provider copy so nothing can
    /// reference a file that is about to be replaced.
    pub fn uploading() -> Self {
        Self {
            state: AttachmentState::Uploading,
            clear_provider: true,
            ..Self::default()
        }
    }

    /// The provider holds a live copy the backend may reference.
    pub fn available(
        provider_file_name: impl Into<String>,
        uploaded_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Self {
        Self {
            state: AttachmentState::Available,
            provider_file_name: Some(provider_file_name.into()),
            uploaded_at: Some(uploaded_at),
            expires_at: Some(expires_at),
            ..Self::default()
        }
    }

    /// The provider copy is gone; the local bytes remain and will be uploaded
    /// again on next use.
    pub fn expired() -> Self {
        Self {
            state: AttachmentState::Expired,
            ..Self::default()
        }
    }

    /// An upload attempt failed; the attachment falls back to needing upload.
    pub fn upload_failed() -> Self {
        Self {
            state: AttachmentState::Imported,
            clear_provider: true,
            ..Self::default()
        }
    }

    /// The attachment will never be used, and this is why.
    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            state: AttachmentState::Rejected,
            rejected_reason: Some(reason.into()),
            clear_provider: true,
            ..Self::default()
        }
    }

    fn apply(&self, metadata: &mut AttachmentMetadata) {
        metadata.state = self.state;
        if self.clear_provider {
            metadata.provider_file_name = None;
            metadata.uploaded_at = None;
            metadata.expires_at = None;
        }
        if self.provider_file_name.is_some() {
            metadata.provider_file_name = self.provider_file_name.clone();
        }
        if self.uploaded_at.is_some() {
            metadata.uploaded_at = self.uploaded_at;
        }
        if self.expires_at.is_some() {
            metadata.expires_at = self.expires_at;
        }
        if self.rejected_reason.is_some() {
            metadata.rejected_reason = self.rejected_reason.clone();
        }
    }
}

/// A job claimed exclusively by a worker until its lease expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    /// Durable job identity.
    pub id: crate::domain::JobId,
    /// Case serialization key.
    pub case_id: Option<CaseId>,
    /// Stable idempotent operation identity.
    pub operation_id: String,
    /// Bounded operation kind.
    pub kind: String,
    /// Opaque JSON handler payload.
    pub payload_json: String,
    /// Attempt number starting at one.
    pub attempts: u32,
    /// Attempts allowed before failure.
    pub max_attempts: u32,
    /// Lease holder token.
    pub lease_owner: String,
    /// Lease deadline.
    pub lease_expires_at: DateTime<Utc>,
}

impl Storage {
    /// Open a SQLite database, enable WAL, and apply all migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(storage_error)?;
        MIGRATOR.run(&pool).await.map_err(migration_error)?;
        restrict_database_files(path.as_ref())?;
        Ok(Self { pool })
    }

    /// Create or reuse a case by its source idempotency key.
    pub async fn create_case_idempotent(
        &self,
        request: &RefinementRequest,
    ) -> Result<CreateCaseResult> {
        request.validate().map_err(|report| AppError::Invalid {
            message: report.to_string(),
        })?;
        let request_json = json(request)?;
        let content_digest = digest(&request_json);
        let now = Utc::now();
        let now_text = timestamp(now);
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        if let Some(row) =
            sqlx::query("SELECT id, content_digest FROM cases WHERE idempotency_key=?")
                .bind(&request.idempotency_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?
        {
            let existing: String = row.get("id");
            if row.get::<String, _>("content_digest") == content_digest {
                tx.commit().await.map_err(storage_error)?;
                return Ok(CreateCaseResult::Existing(parse_case_id(existing)?));
            }
            return Err(AppError::IdempotencyConflict {
                key: request.idempotency_key.clone(),
            });
        }
        let id = CaseId::new();
        sqlx::query("INSERT INTO cases (id,idempotency_key,content_digest,state,request_id,source_json,destination_json,repository_id,metadata_json,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)")
            .bind(id.to_string()).bind(&request.idempotency_key).bind(&content_digest).bind(CaseState::Received.as_str()).bind(&request.request_id).bind(json(&request.source)?).bind(json(&request.destination)?).bind(request.repository.map(|value| value.to_string())).bind(json(&request.metadata)?).bind(&now_text).bind(&now_text).execute(&mut *tx).await.map_err(storage_error)?;
        sqlx::query("INSERT INTO case_inputs (case_id,request_json,transcript_json,task_hint,created_at) VALUES (?,?,?,?,?)").bind(id.to_string()).bind(&request_json).bind(json(&request.transcript)?).bind(&request.task_hint).bind(&now_text).execute(&mut *tx).await.map_err(storage_error)?;
        self.append_event(
            &mut tx,
            id,
            now,
            CaseEventPayload::CaseReceived {
                source_system: source_name(request),
                request_id: request.request_id.clone(),
                attachment_count: request.attachments.len(),
                has_repository: request.repository.is_some(),
            },
        )
        .await?;
        self.enqueue_tx(
            &mut tx,
            QueueSpec {
                case_id: Some(id),
                kind: "prepare",
                operation_id: format!("case:{id}:prepare"),
                payload_json: "{}",
                max_attempts: DEFAULT_MAX_ATTEMPTS,
            },
            now,
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(CreateCaseResult::Created(id))
    }

    /// Record a question request and atomically move the case to awaiting answer.
    pub async fn record_question(&self, request: &QuestionRequest) -> Result<()> {
        request.validate().map_err(|report| AppError::Invalid {
            message: report.to_string(),
        })?;
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, request.case_id).await?;
        let next = transition(
            state,
            &CaseTransition::QuestionRaised {
                question_request_id: request.id,
            },
        )
        .map_err(transition_error)?;
        sqlx::query("INSERT INTO question_requests (id,case_id,status,request_json,created_at) VALUES (?,?,?,?,?)").bind(request.id.to_string()).bind(request.case_id.to_string()).bind("pending").bind(json(request)?).bind(timestamp(now)).execute(&mut *tx).await.map_err(storage_error)?;
        self.set_state_tx(
            &mut tx,
            request.case_id,
            state,
            next,
            CaseTransition::QuestionRaised {
                question_request_id: request.id,
            },
            now,
        )
        .await?;
        self.append_event(
            &mut tx,
            request.case_id,
            now,
            CaseEventPayload::QuestionRequested {
                question_request_id: request.id,
                question_count: request.questions.len(),
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Validate and store answers, then queue the resumable backend operation.
    pub async fn apply_answer(&self, answers: &AnswerSet) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let row = sqlx::query(
            "SELECT request_json,status FROM question_requests WHERE id=? AND case_id=?",
        )
        .bind(answers.question_request_id.to_string())
        .bind(answers.case_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| AppError::NotFound {
            kind: "question_request",
            id: answers.question_request_id.to_string(),
        })?;
        if row.get::<String, _>("status") != "pending" {
            return Err(AppError::Invalid {
                message: "question request is not pending".into(),
            });
        }
        let request: QuestionRequest =
            serde_json::from_str(&row.get::<String, _>("request_json")).map_err(json_error)?;
        request
            .validate_answers(answers)
            .map_err(|report| AppError::Invalid {
                message: report.to_string(),
            })?;
        let state = self.case_state_tx(&mut tx, answers.case_id).await?;
        let next = transition(
            state,
            &CaseTransition::AnswersApplied {
                question_request_id: answers.question_request_id,
            },
        )
        .map_err(transition_error)?;
        for answer in &answers.answers {
            sqlx::query("INSERT INTO answers (question_request_id,case_id,answer_json,created_at) VALUES (?,?,?,?)").bind(answers.question_request_id.to_string()).bind(answers.case_id.to_string()).bind(json(answer)?).bind(timestamp(now)).execute(&mut *tx).await.map_err(storage_error)?;
        }
        sqlx::query("UPDATE question_requests SET status='answered', answered_at=? WHERE id=?")
            .bind(timestamp(now))
            .bind(answers.question_request_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        self.set_state_tx(
            &mut tx,
            answers.case_id,
            state,
            next,
            CaseTransition::AnswersApplied {
                question_request_id: answers.question_request_id,
            },
            now,
        )
        .await?;
        self.append_event(
            &mut tx,
            answers.case_id,
            now,
            CaseEventPayload::AnswersRecorded {
                question_request_id: answers.question_request_id,
                answer_count: answers.answers.len(),
            },
        )
        .await?;
        self.enqueue_tx(
            &mut tx,
            QueueSpec {
                case_id: Some(answers.case_id),
                kind: "run",
                operation_id: format!(
                    "case:{}:resume:{}",
                    answers.case_id, answers.question_request_id
                ),
                payload_json: "{}",
                max_attempts: DEFAULT_MAX_ATTEMPTS,
            },
            now,
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Persist a candidate output and state transition together.
    pub async fn record_output(
        &self,
        case_id: CaseId,
        output_id: OutputId,
        prompt: &crate::domain::RefinedPrompt,
        valid: bool,
        issues: &[String],
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let next = transition(state, &CaseTransition::OutputSubmitted { output_id })
            .map_err(transition_error)?;
        sqlx::query("INSERT INTO outputs (id,case_id,prompt_json,valid,validation_issues_json,created_at) VALUES (?,?,?,?,?,?)").bind(output_id.to_string()).bind(case_id.to_string()).bind(json(prompt)?).bind(valid).bind(json(&issues)?).bind(timestamp(now)).execute(&mut *tx).await.map_err(storage_error)?;
        self.set_state_tx(
            &mut tx,
            case_id,
            state,
            next,
            CaseTransition::OutputSubmitted { output_id },
            now,
        )
        .await?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::OutputRecorded {
                output_id,
                valid,
                issues: issues.to_vec(),
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Reclaim expired leases and queue work implied by every resumable snapshot.
    pub async fn recover_startup(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        sqlx::query("UPDATE jobs SET status='queued',lease_owner=NULL,lease_expires_at=NULL,updated_at=? WHERE status='leased' AND lease_expires_at<=?").bind(timestamp(now)).bind(timestamp(now)).execute(&mut *tx).await.map_err(storage_error)?;
        self.expire_provider_uploads_tx(&mut tx, now).await?;
        let rows = sqlx::query("SELECT id,state FROM cases WHERE state NOT IN ('completed','failed','cancelled','awaiting_answer')").fetch_all(&mut *tx).await.map_err(storage_error)?;
        let count = rows.len();
        for row in rows {
            let id = parse_case_id(row.get("id"))?;
            let kind = match row.get::<String, _>("state").as_str() {
                "received" | "preparing" => "prepare",
                "running" | "validating" => "run",
                "ready" | "delivering" => "deliver",
                _ => continue,
            };
            self.enqueue_tx(
                &mut tx,
                QueueSpec {
                    case_id: Some(id),
                    kind,
                    operation_id: format!("case:{id}:{kind}"),
                    payload_json: "{}",
                    max_attempts: DEFAULT_MAX_ATTEMPTS,
                },
                now,
            )
            .await?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(count)
    }

    /// Mark every provider upload whose expiry has passed.
    ///
    /// A case that waited overnight for a human answer comes back to files the
    /// provider has already collected. Recording that at startup means the
    /// snapshot is honest before anything tries to reference a file that is no
    /// longer there; the bytes are still in the media store, so the next use
    /// simply uploads again.
    async fn expire_provider_uploads_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        let rows = sqlx::query(
            "SELECT id, case_id, metadata_json, media_path FROM attachments WHERE state='available' AND expires_at IS NOT NULL AND expires_at<=?",
        )
        .bind(timestamp(now))
        .fetch_all(&mut **tx)
        .await
        .map_err(storage_error)?;
        let count = rows.len();
        for row in rows {
            let mut stored = stored_attachment(row)?;
            AttachmentStateUpdate::expired().apply(&mut stored.metadata);
            sqlx::query("UPDATE attachments SET metadata_json=?,state=?,updated_at=? WHERE id=?")
                .bind(json(&stored.metadata)?)
                .bind(attachment_state_str(stored.metadata.state))
                .bind(timestamp(now))
                .bind(stored.metadata.id.to_string())
                .execute(&mut **tx)
                .await
                .map_err(storage_error)?;
            self.append_event(
                tx,
                stored.case_id,
                now,
                CaseEventPayload::AttachmentStateChanged {
                    attachment_id: stored.metadata.id,
                    state: AttachmentState::Expired,
                },
            )
            .await?;
        }
        Ok(count)
    }

    /// Claim one due job, excluding any case with a live lease.
    pub async fn claim_job(
        &self,
        owner: &str,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedJob>> {
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let row = sqlx::query("SELECT j.id,j.case_id,j.operation_id,j.kind,j.payload_json,j.attempts,j.max_attempts FROM jobs j WHERE ((j.status='queued' AND j.run_at<=?) OR (j.status='leased' AND j.lease_expires_at<=?)) AND NOT EXISTS (SELECT 1 FROM jobs active WHERE active.case_id=j.case_id AND active.status='leased' AND active.lease_expires_at>? AND active.id<>j.id) ORDER BY j.run_at,j.created_at LIMIT 1").bind(timestamp(now)).bind(timestamp(now)).bind(timestamp(now)).fetch_optional(&mut *tx).await.map_err(storage_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let id = parse_job_id(row.get("id"))?;
        let expiry = now + lease_for;
        sqlx::query("UPDATE jobs SET status='leased',attempts=attempts+1,lease_owner=?,lease_expires_at=?,last_heartbeat_at=?,updated_at=? WHERE id=?").bind(owner).bind(timestamp(expiry)).bind(timestamp(now)).bind(timestamp(now)).bind(id.to_string()).execute(&mut *tx).await.map_err(storage_error)?;
        let job = ClaimedJob {
            id,
            case_id: row
                .get::<Option<String>, _>("case_id")
                .map(parse_case_id)
                .transpose()?,
            operation_id: row.get("operation_id"),
            kind: row.get("kind"),
            payload_json: row.get("payload_json"),
            attempts: row.get::<i64, _>("attempts") as u32 + 1,
            max_attempts: row.get::<i64, _>("max_attempts") as u32,
            lease_owner: owner.into(),
            lease_expires_at: expiry,
        };
        tx.commit().await.map_err(storage_error)?;
        Ok(Some(job))
    }

    /// Renew a lease only if its original worker still owns it.
    pub async fn heartbeat_job(
        &self,
        job: &ClaimedJob,
        lease_for: Duration,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let result = sqlx::query("UPDATE jobs SET lease_expires_at=?,last_heartbeat_at=?,updated_at=? WHERE id=? AND status='leased' AND lease_owner=?").bind(timestamp(now + lease_for)).bind(timestamp(now)).bind(timestamp(now)).bind(job.id.to_string()).bind(&job.lease_owner).execute(&self.pool).await.map_err(storage_error)?;
        Ok(result.rows_affected() == 1)
    }

    /// Complete a leased job only if the owner still matches.
    pub async fn complete_job(&self, job: &ClaimedJob, now: DateTime<Utc>) -> Result<bool> {
        let result = sqlx::query("UPDATE jobs SET status='completed',lease_owner=NULL,lease_expires_at=NULL,completed_at=?,updated_at=? WHERE id=? AND status='leased' AND lease_owner=?").bind(timestamp(now)).bind(timestamp(now)).bind(job.id.to_string()).bind(&job.lease_owner).execute(&self.pool).await.map_err(storage_error)?;
        Ok(result.rows_affected() == 1)
    }

    /// Fail a job with bounded exponential backoff and deterministic jitter.
    pub async fn fail_job(
        &self,
        job: &ClaimedJob,
        error: &AppError,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if error.retry_class().is_retryable() && job.attempts < job.max_attempts {
            let run_at = now + Duration::seconds(backoff_seconds(&job.operation_id, job.attempts));
            sqlx::query("UPDATE jobs SET status='queued',run_at=?,lease_owner=NULL,lease_expires_at=NULL,last_error=?,updated_at=? WHERE id=? AND status='leased' AND lease_owner=?").bind(timestamp(run_at)).bind(error.to_string()).bind(timestamp(now)).bind(job.id.to_string()).bind(&job.lease_owner).execute(&self.pool).await.map_err(storage_error)?;
        } else {
            sqlx::query("UPDATE jobs SET status='failed',lease_owner=NULL,lease_expires_at=NULL,last_error=?,updated_at=? WHERE id=? AND status='leased' AND lease_owner=?").bind(error.to_string()).bind(timestamp(now)).bind(job.id.to_string()).bind(&job.lease_owner).execute(&self.pool).await.map_err(storage_error)?;
        }
        Ok(())
    }

    /// Read a snapshot's current state.
    pub async fn case_state(&self, id: CaseId) -> Result<CaseState> {
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let result = self.case_state_tx(&mut tx, id).await;
        tx.commit().await.map_err(storage_error)?;
        result
    }

    /// Read a case snapshot without exposing its untrusted inputs or secrets.
    pub async fn case_summary(&self, id: CaseId) -> Result<CaseSummary> {
        let row =
            sqlx::query("SELECT id,state,request_id,created_at,updated_at FROM cases WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(storage_error)?
                .ok_or_else(|| AppError::NotFound {
                    kind: "case",
                    id: id.to_string(),
                })?;
        Ok(CaseSummary {
            id: parse_case_id(row.get("id"))?,
            state: parse_state(row.get("state"))?,
            request_id: row.get("request_id"),
            created_at: parse_time(row.get("created_at"))?,
            updated_at: parse_time(row.get("updated_at"))?,
        })
    }

    /// List recent cases, optionally restricted to one state. The API keeps
    /// this deliberately bounded so a local UI cannot accidentally load an
    /// unbounded history into memory.
    pub async fn list_case_summaries(
        &self,
        state: Option<CaseState>,
        limit: u32,
    ) -> Result<Vec<CaseSummary>> {
        let limit = i64::from(limit.clamp(1, 200));
        let rows = if let Some(state) = state {
            sqlx::query("SELECT id,state,request_id,created_at,updated_at FROM cases WHERE state=? ORDER BY created_at DESC LIMIT ?").bind(state.as_str()).bind(limit).fetch_all(&self.pool).await
        } else {
            sqlx::query("SELECT id,state,request_id,created_at,updated_at FROM cases ORDER BY created_at DESC LIMIT ?").bind(limit).fetch_all(&self.pool).await
        }.map_err(storage_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(CaseSummary {
                    id: parse_case_id(row.get("id"))?,
                    state: parse_state(row.get("state"))?,
                    request_id: row.get("request_id"),
                    created_at: parse_time(row.get("created_at"))?,
                    updated_at: parse_time(row.get("updated_at"))?,
                })
            })
            .collect()
    }

    async fn case_state_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        id: CaseId,
    ) -> Result<CaseState> {
        let row = sqlx::query("SELECT state FROM cases WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut **tx)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| AppError::NotFound {
                kind: "case",
                id: id.to_string(),
            })?;
        parse_state(row.get("state"))
    }
    async fn set_state_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        id: CaseId,
        from: CaseState,
        to: CaseState,
        event_transition: CaseTransition,
        now: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query("UPDATE cases SET state=?,updated_at=?,completed_at=CASE WHEN ? IN ('completed','failed','cancelled') THEN ? ELSE completed_at END WHERE id=?").bind(to.as_str()).bind(timestamp(now)).bind(to.as_str()).bind(timestamp(now)).bind(id.to_string()).execute(&mut **tx).await.map_err(storage_error)?;
        self.append_event(
            tx,
            id,
            now,
            CaseEventPayload::StateChanged {
                from,
                to,
                transition: event_transition,
            },
        )
        .await
    }
    async fn append_event(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        case_id: CaseId,
        now: DateTime<Utc>,
        payload: CaseEventPayload,
    ) -> Result<()> {
        let sequence = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM case_events WHERE case_id=?",
        )
        .bind(case_id.to_string())
        .fetch_one(&mut **tx)
        .await
        .map_err(storage_error)? as u64;
        let event = CaseEvent::new(case_id, sequence, now, payload);
        sqlx::query("INSERT INTO case_events (id,case_id,sequence,occurred_at,type,payload_json) VALUES (?,?,?,?,?,?)").bind(event.id.to_string()).bind(case_id.to_string()).bind(sequence as i64).bind(timestamp(now)).bind(event.payload.type_str()).bind(json(&event)?).execute(&mut **tx).await.map_err(storage_error)?;
        Ok(())
    }
    /// Register a repository root, or return the existing registration for a
    /// root that is already registered.
    ///
    /// Registration is keyed on the canonical path, so `refinery repository
    /// add .` run twice, or run once from a symlink and once from the real
    /// path, produces one repository rather than two views of the same tree
    /// with divergent policies. Re-registering refreshes the policy, which is
    /// how a user picks up new limits without unregistering first.
    pub async fn register_repository(
        &self,
        root: &Path,
        policy: &crate::repositories::RepositoryPolicy,
        now: DateTime<Utc>,
    ) -> Result<(crate::repositories::Repository, bool)> {
        let key = root.to_string_lossy().into_owned();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;

        let existing = sqlx::query("SELECT id,created_at FROM repositories WHERE canonical_path=?")
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage_error)?;

        let (id, registered_at, created) = match existing {
            Some(row) => {
                let id = parse_repository_id(row.get("id"))?;
                sqlx::query("UPDATE repositories SET policy_json=?,updated_at=? WHERE id=?")
                    .bind(json(policy)?)
                    .bind(timestamp(now))
                    .bind(id.to_string())
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                (id, parse_time(row.get("created_at"))?, false)
            }
            None => {
                let id = crate::domain::RepositoryId::new();
                sqlx::query("INSERT INTO repositories (id,canonical_path,policy_json,created_at,updated_at) VALUES (?,?,?,?,?)")
                    .bind(id.to_string())
                    .bind(&key)
                    .bind(json(policy)?)
                    .bind(timestamp(now))
                    .bind(timestamp(now))
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
                (id, now, true)
            }
        };

        tx.commit().await.map_err(storage_error)?;
        Ok((
            crate::repositories::Repository {
                id,
                root: root.to_path_buf(),
                policy: policy.clone(),
                registered_at,
            },
            created,
        ))
    }

    /// Every registered repository, oldest registration first.
    pub async fn list_repositories(&self) -> Result<Vec<crate::repositories::Repository>> {
        let rows = sqlx::query(
            "SELECT id,canonical_path,policy_json,created_at FROM repositories ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;

        rows.into_iter()
            .map(|row| {
                Ok(crate::repositories::Repository {
                    id: parse_repository_id(row.get("id"))?,
                    root: std::path::PathBuf::from(row.get::<String, _>("canonical_path")),
                    policy: serde_json::from_str(row.get("policy_json")).map_err(json_error)?,
                    registered_at: parse_time(row.get("created_at"))?,
                })
            })
            .collect()
    }

    /// Remove a repository registration by canonical path.
    ///
    /// Returns whether anything was removed, so a caller can tell "no longer
    /// registered" from "was never registered".
    pub async fn forget_repository(&self, root: &Path) -> Result<bool> {
        let result = sqlx::query("DELETE FROM repositories WHERE canonical_path=?")
            .bind(root.to_string_lossy().into_owned())
            .execute(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }

    /// Append the audit record required for every repository-tool attempt.
    ///
    /// Repository reads do not change a case snapshot, but they are still
    /// durable security-relevant facts. The connector calls this for both
    /// successful and refused operations, so a traversal attempt cannot
    /// disappear simply because no bytes were returned.
    pub async fn record_repository_tool_invocation(
        &self,
        case_id: CaseId,
        tool: &str,
        arguments_digest: &str,
        result_bytes: usize,
        outcome: crate::domain::Outcome,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::RepositoryToolInvoked {
                tool: tool.to_owned(),
                arguments_digest: arguments_digest.to_owned(),
                result_bytes,
                outcome,
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Record an attachment that passed import, with its bytes already in the
    /// case's media directory.
    ///
    /// The row and the `attachment_imported` event are written together, so a
    /// case's history can never show media the snapshot does not have or hold
    /// media the history cannot explain.
    pub async fn record_imported_attachment(
        &self,
        case_id: CaseId,
        imported: &crate::media::ImportedAttachment,
    ) -> Result<()> {
        let now = Utc::now();
        let metadata = &imported.metadata;
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        self.case_state_tx(&mut tx, case_id).await?;
        sqlx::query("INSERT INTO attachments (id,case_id,metadata_json,state,media_path,digest_sha256,provider_file_name,uploaded_at,expires_at,rejected_reason,created_at,updated_at) VALUES (?,?,?,?,?,?,NULL,NULL,NULL,NULL,?,?)")
            .bind(metadata.id.to_string())
            .bind(case_id.to_string())
            .bind(json(metadata)?)
            .bind(attachment_state_str(metadata.state))
            .bind(imported.path.to_string_lossy().into_owned())
            .bind(&metadata.digest_sha256)
            .bind(timestamp(now))
            .bind(timestamp(now))
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::AttachmentImported {
                attachment_id: metadata.id,
                media_type: metadata.media_type.clone(),
                size_bytes: metadata.size_bytes,
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Record an attachment that import refused.
    ///
    /// A refusal is durable rather than merely returned, because "the video
    /// you attached never reached the model" is exactly the question a user
    /// asks later, and a case with a silent gap cannot answer it. There are no
    /// bytes and no digest, only the declaration and the reason.
    pub async fn record_rejected_attachment(
        &self,
        case_id: CaseId,
        input: &crate::domain::AttachmentInput,
        rejection: &crate::media::ImportRejection,
    ) -> Result<AttachmentId> {
        let now = Utc::now();
        let metadata = AttachmentMetadata {
            id: AttachmentId::new(),
            name: bounded_name(&input.name),
            kind: input.kind,
            media_type: bounded_name(&input.media_type),
            size_bytes: input.size_bytes.unwrap_or_default(),
            digest_sha256: input.digest_sha256.clone().unwrap_or_default(),
            state: AttachmentState::Rejected,
            provider_file_name: None,
            uploaded_at: None,
            expires_at: None,
            rejected_reason: Some(rejection.to_string()),
        };
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        self.case_state_tx(&mut tx, case_id).await?;
        sqlx::query("INSERT INTO attachments (id,case_id,metadata_json,state,media_path,digest_sha256,provider_file_name,uploaded_at,expires_at,rejected_reason,created_at,updated_at) VALUES (?,?,?,?,NULL,NULL,NULL,NULL,NULL,?,?,?)")
            .bind(metadata.id.to_string())
            .bind(case_id.to_string())
            .bind(json(&metadata)?)
            .bind(attachment_state_str(metadata.state))
            .bind(metadata.rejected_reason.clone())
            .bind(timestamp(now))
            .bind(timestamp(now))
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::AttachmentStateChanged {
                attachment_id: metadata.id,
                state: AttachmentState::Rejected,
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(metadata.id)
    }

    /// Move an attachment to a new lifecycle state, recording the event.
    ///
    /// Provider fields are overwritten only when the update names them, so a
    /// transition to `expired` keeps the provider name that expired — useful
    /// when reading history — while a transition to `uploading` clears it,
    /// because a stale provider reference during a re-upload is exactly the
    /// thing that would be handed to a model by mistake.
    pub async fn record_attachment_state(
        &self,
        id: AttachmentId,
        update: AttachmentStateUpdate,
    ) -> Result<StoredAttachment> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let mut stored = self.attachment_tx(&mut tx, id).await?;
        update.apply(&mut stored.metadata);
        sqlx::query("UPDATE attachments SET metadata_json=?,state=?,provider_file_name=?,uploaded_at=?,expires_at=?,rejected_reason=?,updated_at=? WHERE id=?")
            .bind(json(&stored.metadata)?)
            .bind(attachment_state_str(stored.metadata.state))
            .bind(stored.metadata.provider_file_name.clone())
            .bind(stored.metadata.uploaded_at.map(timestamp))
            .bind(stored.metadata.expires_at.map(timestamp))
            .bind(stored.metadata.rejected_reason.clone())
            .bind(timestamp(now))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        self.append_event(
            &mut tx,
            stored.case_id,
            now,
            CaseEventPayload::AttachmentStateChanged {
                attachment_id: id,
                state: stored.metadata.state,
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(stored)
    }

    /// Read one attachment.
    pub async fn attachment(&self, id: AttachmentId) -> Result<StoredAttachment> {
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let result = self.attachment_tx(&mut tx, id).await;
        tx.commit().await.map_err(storage_error)?;
        result
    }

    /// Read a case's attachments in import order.
    pub async fn case_attachments(&self, case_id: CaseId) -> Result<Vec<StoredAttachment>> {
        let rows = sqlx::query(
            "SELECT id, case_id, metadata_json, media_path FROM attachments WHERE case_id=? ORDER BY id",
        )
        .bind(case_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        rows.into_iter().map(stored_attachment).collect()
    }

    async fn attachment_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        id: AttachmentId,
    ) -> Result<StoredAttachment> {
        let row = sqlx::query(
            "SELECT id, case_id, metadata_json, media_path FROM attachments WHERE id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| AppError::NotFound {
            kind: "attachment",
            id: id.to_string(),
        })?;
        stored_attachment(row)
    }

    /// Return a case's ordered immutable event history.
    pub async fn case_events(&self, case_id: CaseId) -> Result<Vec<CaseEvent>> {
        let rows =
            sqlx::query("SELECT payload_json FROM case_events WHERE case_id=? ORDER BY sequence")
                .bind(case_id.to_string())
                .fetch_all(&self.pool)
                .await
                .map_err(storage_error)?;
        rows.into_iter()
            .map(|row| {
                serde_json::from_str(&row.get::<String, _>("payload_json")).map_err(json_error)
            })
            .collect()
    }

    /// How many cases sit in each state, for `status` and the health view.
    pub async fn case_counts_by_state(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        let rows = sqlx::query("SELECT state, COUNT(*) AS total FROM cases GROUP BY state")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get::<String, _>("state"),
                    row.get::<i64, _>("total") as u64,
                )
            })
            .collect())
    }

    /// How many jobs sit in each status, for `status` and the health view.
    pub async fn job_counts_by_status(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        let rows = sqlx::query("SELECT status, COUNT(*) AS total FROM jobs GROUP BY status")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get::<String, _>("status"),
                    row.get::<i64, _>("total") as u64,
                )
            })
            .collect())
    }

    /// Product success measures computed from the durable case record.
    ///
    /// The answer-to-resume numerator comes from the immutable transition
    /// event rather than the current case state: a case that resumed and later
    /// completed still counts, and an answered row without its paired state
    /// transition would be visible as a reliability failure.
    pub async fn product_metrics(&self) -> Result<ProductMetrics> {
        let observed = self.case_counts_by_state().await?;
        let mut case_funnel = std::collections::BTreeMap::new();
        for state in CaseState::ALL {
            case_funnel.insert(
                state.as_str().to_owned(),
                observed.get(state.as_str()).copied().unwrap_or_default(),
            );
        }
        let submitted_cases = case_funnel.values().sum();
        let delivered_cases = case_funnel.get("completed").copied().unwrap_or_default();

        let cases_with_questions: i64 =
            sqlx::query_scalar("SELECT COUNT(DISTINCT case_id) FROM question_requests")
                .fetch_one(&self.pool)
                .await
                .map_err(storage_error)?;
        let question_requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM question_requests")
            .fetch_one(&self.pool)
            .await
            .map_err(storage_error)?;
        let answered_question_requests: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM question_requests WHERE status='answered'")
                .fetch_one(&self.pool)
                .await
                .map_err(storage_error)?;
        let resumed_answers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM case_events WHERE type='state_changed' \
             AND json_extract(payload_json, '$.transition.kind')='answers_applied'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(storage_error)?;

        let delivery_row = sqlx::query(
            "SELECT COUNT(*) AS deliveries, COALESCE(SUM(attempt), 0) AS attempts FROM deliveries",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(storage_error)?;
        let deliveries = nonnegative(delivery_row.get::<i64, _>("deliveries"));
        let delivery_attempts = nonnegative(delivery_row.get::<i64, _>("attempts"));

        let output_row = sqlx::query(
            "SELECT COUNT(*) AS candidates, \
             COALESCE(SUM(CASE WHEN valid=0 THEN 1 ELSE 0 END), 0) AS failures FROM outputs",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(storage_error)?;

        Ok(ProductMetrics {
            case_funnel,
            submitted_cases,
            delivered_cases,
            cases_with_questions: nonnegative(cases_with_questions),
            question_requests: nonnegative(question_requests),
            answered_question_requests: nonnegative(answered_question_requests),
            resumed_answers: nonnegative(resumed_answers),
            deliveries,
            delivery_attempts,
            delivery_retries: delivery_attempts.saturating_sub(deliveries),
            candidate_outputs: nonnegative(output_row.get::<i64, _>("candidates")),
            validation_failures: nonnegative(output_row.get::<i64, _>("failures")),
        })
    }

    /// Whether every committed migration has been applied.
    ///
    /// [`Storage::open`] applies migrations, so a mismatch here means the
    /// database was written by a different build — which is a `doctor` finding,
    /// not something to repair silently.
    pub async fn pending_migrations(&self) -> Result<Vec<i64>> {
        let applied: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(MIGRATOR
            .migrations
            .iter()
            .map(|migration| migration.version)
            .filter(|version| !applied.contains(version))
            .collect())
    }

    /// Run SQLite's own integrity check, returning the problems it found.
    ///
    /// An empty result means the database is sound. This is the one check that
    /// can catch a corrupted file before a case is lost to it.
    pub async fn integrity_problems(&self) -> Result<Vec<String>> {
        let rows: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(rows
            .into_iter()
            .filter(|line| !line.eq_ignore_ascii_case("ok"))
            .collect())
    }

    async fn enqueue_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        spec: QueueSpec<'_>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let id = crate::domain::JobId::new();
        sqlx::query("INSERT INTO jobs (id,case_id,operation_id,kind,payload_json,status,attempts,max_attempts,run_at,created_at,updated_at) VALUES (?,?,?,?,?,'queued',0,?,?,?,?) ON CONFLICT(operation_id) DO UPDATE SET status=CASE WHEN jobs.status IN ('completed','failed') THEN 'queued' ELSE jobs.status END,run_at=CASE WHEN jobs.status IN ('completed','failed') THEN excluded.run_at ELSE jobs.run_at END,updated_at=excluded.updated_at").bind(id.to_string()).bind(spec.case_id.map(|value| value.to_string())).bind(spec.operation_id).bind(spec.kind).bind(spec.payload_json).bind(spec.max_attempts as i64).bind(timestamp(now)).bind(timestamp(now)).bind(timestamp(now)).execute(&mut **tx).await.map_err(storage_error)?;
        Ok(())
    }
}

/// Restrict the database and its journal companions to the owner.
///
/// SQLite creates its files with the process umask, which on a typical machine
/// means `0644`. The data directory itself is `0700`, so nothing is exposed
/// today — but the database holds transcripts, refined prompts, and repository
/// excerpts, and it should not depend on one directory's mode to stay private.
/// A copy of the file taken anywhere else carries its own permissions.
fn restrict_database_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let companion = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            std::path::PathBuf::from(name)
        };
        if companion.exists() {
            crate::config::paths::set_private_file_mode(&companion)?;
        }
    }
    Ok(())
}

fn storage_error(error: sqlx::Error) -> AppError {
    AppError::Storage {
        message: error.to_string(),
        retry: RetryClass::Retryable,
    }
}
fn migration_error(error: sqlx::migrate::MigrateError) -> AppError {
    AppError::Storage {
        message: error.to_string(),
        retry: RetryClass::NonRetryable,
    }
}
fn json_error(error: serde_json::Error) -> AppError {
    AppError::Storage {
        message: format!("stored JSON is invalid: {error}"),
        retry: RetryClass::NonRetryable,
    }
}
fn transition_error(error: crate::cases::TransitionError) -> AppError {
    AppError::Invalid {
        message: error.to_string(),
    }
}
fn json(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(json_error)
}
fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
/// The wire spelling of an attachment state, used for the indexed column.
fn attachment_state_str(state: AttachmentState) -> &'static str {
    match state {
        AttachmentState::Imported => "imported",
        AttachmentState::Uploading => "uploading",
        AttachmentState::Available => "available",
        AttachmentState::Expired => "expired",
        AttachmentState::Rejected => "rejected",
    }
}
/// Bound a submitter-supplied string before it is stored on a rejected
/// attachment. The declaration failed validation, so its length is not
/// something the contract has already guaranteed.
fn bounded_name(value: &str) -> String {
    let mut value = value.to_owned();
    let limit = crate::domain::limits::MAX_ATTACHMENT_NAME_CHARS;
    if value.chars().count() > limit {
        value = value.chars().take(limit).collect();
    }
    value
}
fn stored_attachment(row: sqlx::sqlite::SqliteRow) -> Result<StoredAttachment> {
    Ok(StoredAttachment {
        case_id: parse_case_id(row.get("case_id"))?,
        metadata: serde_json::from_str(&row.get::<String, _>("metadata_json"))
            .map_err(json_error)?,
        media_path: row
            .get::<Option<String>, _>("media_path")
            .map(std::path::PathBuf::from),
    })
}
fn parse_case_id(value: String) -> Result<CaseId> {
    value
        .parse()
        .map_err(|error: crate::domain::InvalidId| AppError::Storage {
            message: error.to_string(),
            retry: RetryClass::NonRetryable,
        })
}
fn parse_repository_id(value: String) -> Result<crate::domain::RepositoryId> {
    value
        .parse()
        .map_err(|error: crate::domain::InvalidId| AppError::Storage {
            message: error.to_string(),
            retry: RetryClass::NonRetryable,
        })
}
fn parse_time(value: String) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| AppError::Storage {
            message: format!("stored timestamp is invalid: {error}"),
            retry: RetryClass::NonRetryable,
        })
}
fn parse_job_id(value: String) -> Result<crate::domain::JobId> {
    value
        .parse()
        .map_err(|error: crate::domain::InvalidId| AppError::Storage {
            message: error.to_string(),
            retry: RetryClass::NonRetryable,
        })
}
fn parse_state(value: String) -> Result<CaseState> {
    CaseState::ALL
        .iter()
        .copied()
        .find(|state| state.as_str() == value)
        .ok_or_else(|| AppError::Storage {
            message: format!("invalid stored case state {value}"),
            retry: RetryClass::NonRetryable,
        })
}
fn source_name(request: &RefinementRequest) -> String {
    match &request.source.system {
        crate::domain::SourceSystem::Overlord => "overlord".into(),
        crate::domain::SourceSystem::LocalUi => "local_ui".into(),
        crate::domain::SourceSystem::Cli => "cli".into(),
        crate::domain::SourceSystem::Other(value) => value.clone(),
    }
}
fn nonnegative(value: i64) -> u64 {
    value.max(0) as u64
}
fn backoff_seconds(operation_id: &str, attempts: u32) -> i64 {
    let base = 1_i64 << attempts.saturating_sub(1).min(8);
    let jitter =
        Sha256::digest(format!("{operation_id}:{attempts}").as_bytes())[0] as i64 % (base + 1);
    (base + jitter).min(300)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    async fn store() -> Storage {
        let path = tempdir().unwrap().keep().join("refinery.db");
        Storage::open(path).await.unwrap()
    }
    fn request() -> RefinementRequest {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/refinement_request.json"
        ))
        .unwrap()
    }
    #[tokio::test]
    async fn idempotent_ingress_returns_original_and_rejects_conflict() {
        let store = store().await;
        let first = store.create_case_idempotent(&request()).await.unwrap();
        assert!(matches!(first, CreateCaseResult::Created(_)));
        assert_eq!(
            store.create_case_idempotent(&request()).await.unwrap(),
            CreateCaseResult::Existing(first.case_id())
        );
        let mut changed = request();
        changed.task_hint = Some("different".into());
        assert!(matches!(
            store.create_case_idempotent(&changed).await,
            Err(AppError::IdempotencyConflict { .. })
        ));
    }

    #[tokio::test]
    async fn product_metrics_are_reconstructed_from_durable_facts() {
        let store = store().await;
        let case_id = store
            .create_case_idempotent(&request())
            .await
            .unwrap()
            .case_id();
        store.mark_preparation_started(case_id).await.unwrap();
        store.mark_backend_started(case_id).await.unwrap();

        let mut question: crate::domain::QuestionRequest = serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/question_request.json"
        ))
        .unwrap();
        question.id = crate::domain::QuestionRequestId::new();
        question.case_id = case_id;
        store.record_question(&question).await.unwrap();
        let mut answer: crate::domain::AnswerSet = serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/answer_set.json"
        ))
        .unwrap();
        answer.case_id = case_id;
        answer.question_request_id = question.id;
        store.apply_answer(&answer).await.unwrap();

        let prompt: crate::domain::RefinedPrompt = serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/refined_prompt.json"
        ))
        .unwrap();
        let rejected = OutputId::new();
        store
            .record_output(
                case_id,
                rejected,
                &prompt,
                false,
                &["acceptance criteria missing".into()],
            )
            .await
            .unwrap();
        store
            .reject_output_for_retry(case_id, rejected)
            .await
            .unwrap();
        let accepted = OutputId::new();
        store
            .record_output(case_id, accepted, &prompt, true, &[])
            .await
            .unwrap();
        store.accept_output(case_id, accepted).await.unwrap();
        let first = store.begin_delivery(case_id).await.unwrap();
        store
            .record_delivery_failure(
                case_id,
                first.delivery_id,
                RetryClass::Retryable,
                "temporary outage",
            )
            .await
            .unwrap();
        let second = store.begin_delivery(case_id).await.unwrap();
        assert_eq!(first.delivery_id, second.delivery_id);
        store
            .complete_delivery(
                case_id,
                second.delivery_id,
                &crate::domain::DeliveryReceipt {
                    schema_version: crate::domain::SchemaVersion::CURRENT,
                    accepted: true,
                    destination_reference: Some("coo:885.metrics".into()),
                    received_at: Some(Utc::now()),
                    message: None,
                },
            )
            .await
            .unwrap();

        let metrics = store.product_metrics().await.unwrap();
        assert_eq!(metrics.submitted_cases, 1);
        assert_eq!(metrics.delivered_cases, 1);
        assert_eq!(metrics.cases_with_questions, 1);
        assert_eq!(metrics.question_requests, 1);
        assert_eq!(metrics.answered_question_requests, 1);
        assert_eq!(metrics.resumed_answers, 1);
        assert_eq!(metrics.deliveries, 1);
        assert_eq!(metrics.delivery_attempts, 2);
        assert_eq!(metrics.delivery_retries, 1);
        assert_eq!(metrics.candidate_outputs, 2);
        assert_eq!(metrics.validation_failures, 1);
        assert_eq!(metrics.case_funnel.get("completed"), Some(&1));
        assert_eq!(metrics.case_funnel.len(), CaseState::ALL.len());
    }
    #[tokio::test]
    async fn expired_lease_is_reclaimed() {
        let store = store().await;
        store.create_case_idempotent(&request()).await.unwrap();
        let now = Utc::now();
        let first = store
            .claim_job("first", Duration::seconds(1), now)
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .claim_job("second", Duration::seconds(1), now)
            .await
            .unwrap()
            .is_none());
        store
            .recover_startup(now + Duration::seconds(2))
            .await
            .unwrap();
        let second = store
            .claim_job("second", Duration::seconds(30), now + Duration::seconds(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.attempts, 2);
    }
    #[tokio::test]
    async fn concurrent_workers_never_claim_same_case() {
        let store = store().await;
        store.create_case_idempotent(&request()).await.unwrap();
        let now = Utc::now();
        let (left, right) = tokio::join!(
            store.claim_job("left", Duration::seconds(30), now),
            store.claim_job("right", Duration::seconds(30), now)
        );
        assert_eq!(
            [left.unwrap().is_some(), right.unwrap().is_some()]
                .into_iter()
                .filter(|value| *value)
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn startup_recovery_requeues_crashed_job() {
        let store = store().await;
        let now = Utc::now();
        store.create_case_idempotent(&request()).await.unwrap();
        store
            .claim_job("crashed", Duration::seconds(1), now)
            .await
            .unwrap();
        store
            .recover_startup(now + Duration::seconds(2))
            .await
            .unwrap();
        assert!(store
            .claim_job(
                "restarted",
                Duration::seconds(30),
                now + Duration::seconds(2)
            )
            .await
            .unwrap()
            .is_some());
    }
    #[tokio::test]
    async fn startup_recovery_marks_provider_uploads_that_expired_while_we_were_down() {
        let store = store().await;
        let now = Utc::now();
        let case_id = store
            .create_case_idempotent(&request())
            .await
            .unwrap()
            .case_id();
        let dir = tempdir().unwrap().keep();
        let path = dir.join("shot.png");
        let bytes = b"\x89PNG\r\n\x1a\n";
        std::fs::write(&path, bytes).unwrap();
        let imported = crate::media::ImportedAttachment {
            metadata: AttachmentMetadata {
                id: AttachmentId::new(),
                name: "shot.png".into(),
                kind: crate::domain::AttachmentKind::Image,
                media_type: "image/png".into(),
                size_bytes: bytes.len() as u64,
                digest_sha256: crate::media::hex_digest(bytes),
                state: AttachmentState::Imported,
                provider_file_name: None,
                uploaded_at: None,
                expires_at: None,
                rejected_reason: None,
            },
            path,
        };
        let id = imported.metadata.id;
        store
            .record_imported_attachment(case_id, &imported)
            .await
            .unwrap();
        store
            .record_attachment_state(
                id,
                AttachmentStateUpdate::available("files/abc", now, now + Duration::hours(1)),
            )
            .await
            .unwrap();

        // Restarting inside the expiry window leaves the upload alone.
        store.recover_startup(now).await.unwrap();
        assert_eq!(
            store.attachment(id).await.unwrap().metadata.state,
            AttachmentState::Available
        );

        // Restarting after it lapsed records the expiry, and the local bytes
        // are still there for the re-upload.
        store
            .recover_startup(now + Duration::hours(2))
            .await
            .unwrap();
        let stored = store.attachment(id).await.unwrap();
        assert_eq!(stored.metadata.state, AttachmentState::Expired);
        assert!(stored.metadata.state.needs_upload());
        assert!(stored.media_path.is_some());
        assert!(store
            .case_events(case_id)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(
                event.payload,
                CaseEventPayload::AttachmentStateChanged {
                    state: AttachmentState::Expired,
                    ..
                }
            )));
    }
}
