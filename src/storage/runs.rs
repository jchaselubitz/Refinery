//! Backend runs and the run-phase case intents.
//!
//! A *run* is one provider conversation for one case. Gemini's API is
//! stateless per request, so the conversation only exists because Refinery
//! writes it down: [`Storage::save_run_step`] persists the canonical history
//! after every step, and [`Storage::open_run`] hands it back on resume. That
//! is what "resumable conversation" means for this backend — replay, not a
//! provider-side session — and it is why a case can wait overnight for a human
//! answer, survive a restart in between, and continue from exactly where the
//! model left off.
//!
//! The run intents live beside the case transitions they always accompany.
//! Accepting an output, sending one back for its single corrective retry, and
//! failing a case are all snapshot mutations paired with an event append in one
//! transaction, exactly like the intents in the parent module.

use chrono::Utc;
use sqlx::Row;

use super::{
    json, json_error, parse_case_id, storage_error, timestamp, transition_error, QueueSpec,
    Storage, DEFAULT_MAX_ATTEMPTS,
};
use crate::{
    cases::transition,
    domain::{
        Answer, AnswerSet, CaseEventPayload, CaseId, CaseState, CaseTransition,
        DeliveredAttachment, DeliveryEnvelope, DeliveryId, DeliveryReceipt, Outcome, OutputId,
        QuestionRequest, RefinementRequest, RunId, SchemaVersion, UsageMetadata,
    },
    error::{AppError, Result, RetryClass},
};

/// Where a backend run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// The conversation is live: it is either running now or waiting for an
    /// answer that will resume it.
    Active,
    /// The conversation produced its output and will not be extended.
    Finished,
    /// The conversation stopped without an accepted output.
    Failed,
}

/// A durably started delivery attempt ready for its destination adapter.
#[derive(Debug, Clone)]
pub struct DeliveryWork {
    /// Recorded delivery identity.
    pub delivery_id: DeliveryId,
    /// The versioned outbound envelope.
    pub envelope: DeliveryEnvelope,
}

impl RunStatus {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Active => "active",
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(RunStatus::Active),
            "finished" => Ok(RunStatus::Finished),
            "failed" => Ok(RunStatus::Failed),
            other => Err(AppError::Storage {
                message: format!("invalid stored run status {other}"),
                retry: RetryClass::NonRetryable,
            }),
        }
    }
}

/// One provider conversation, as storage holds it.
///
/// `history` is the backend's own wire representation, kept opaque here so the
/// storage layer never has to be taught a provider's message format. What
/// storage guarantees is that whatever the backend wrote last is what it reads
/// back, whether the gap between the two was a millisecond or a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRun {
    /// Durable run identity.
    pub id: RunId,
    /// The case this conversation belongs to.
    pub case_id: CaseId,
    /// Which backend is driving it, for example `gemini`.
    pub backend_id: String,
    /// Whether the conversation is still live.
    pub status: RunStatus,
    /// How many provider calls have completed.
    pub step: u32,
    /// The backend's canonical conversation history.
    pub history: serde_json::Value,
    /// Usage accumulated across the conversation, when the provider reports it.
    pub usage: Option<UsageMetadata>,
}

impl Storage {
    /// Persist an attempt before handing the envelope to the destination.
    ///
    /// This is the single entry point for both halves of delivery, because
    /// they differ only in bookkeeping. A case in `ready`, or in `failed` with
    /// its output preserved and a person asking for another try, starts a new
    /// delivery: a new identity, attempt one, and a `DeliveryStarted`
    /// transition. A case already in `delivering` is on a scheduled retry of
    /// the *same* delivery, so it keeps its identity, increments the attempt,
    /// and changes no state — the state machine says a failed attempt leaves
    /// the case where it is, and re-announcing a start it never left would
    /// contradict that.
    ///
    /// Either way the envelope carries a per-attempt idempotency key derived
    /// from the delivery identity and the attempt number, so the destination
    /// can tell a retry of one submission from a second submission.
    pub async fn begin_delivery(&self, case_id: CaseId) -> Result<DeliveryWork> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        if state == CaseState::Delivering {
            let work = self.continue_delivery_tx(&mut tx, case_id, now).await?;
            tx.commit().await.map_err(storage_error)?;
            return Ok(work);
        }
        let output_id = self.accepted_output_id_tx(&mut tx, case_id).await?;
        let destination_kind = self.destination_kind_tx(&mut tx, case_id).await?;
        let delivery_id = DeliveryId::new();
        let delivery_transition = CaseTransition::DeliveryStarted { delivery_id };
        let next = transition(state, &delivery_transition).map_err(transition_error)?;
        let envelope = self
            .delivery_envelope_tx(&mut tx, case_id, delivery_id, output_id, 1, now)
            .await?;
        sqlx::query("INSERT INTO deliveries (id,case_id,output_id,destination_kind,attempt,status,idempotency_key,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?)").bind(delivery_id.to_string()).bind(case_id.to_string()).bind(output_id.to_string()).bind(&destination_kind).bind(1_i64).bind("in_flight").bind(&envelope.idempotency_key).bind(timestamp(now)).bind(timestamp(now)).execute(&mut *tx).await.map_err(storage_error)?;
        self.set_state_tx(&mut tx, case_id, state, next, delivery_transition, now)
            .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(DeliveryWork {
            delivery_id,
            envelope,
        })
    }

    /// Build the outbound envelope for one attempt of one delivery.
    ///
    /// Both the first attempt and every retry come through here so that the
    /// prompt, the attachment manifest, and the repository and metadata
    /// references a destination sees cannot drift between attempts of the same
    /// submission. Only the idempotency key changes, and it changes by rule.
    async fn delivery_envelope_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        case_id: CaseId,
        delivery_id: DeliveryId,
        output_id: OutputId,
        attempt: u32,
        now: chrono::DateTime<Utc>,
    ) -> Result<DeliveryEnvelope> {
        let request = self.case_request_tx(tx, case_id).await?;
        let prompt_json: String =
            sqlx::query_scalar("SELECT prompt_json FROM outputs WHERE id=? AND case_id=?")
                .bind(output_id.to_string())
                .bind(case_id.to_string())
                .fetch_optional(&mut **tx)
                .await
                .map_err(storage_error)?
                .ok_or_else(|| AppError::NotFound {
                    kind: "output",
                    id: output_id.to_string(),
                })?;
        let attachments = sqlx::query(
            "SELECT metadata_json FROM attachments WHERE case_id=? AND state!='rejected'",
        )
        .bind(case_id.to_string())
        .fetch_all(&mut **tx)
        .await
        .map_err(storage_error)?
        .into_iter()
        .map(|row| {
            serde_json::from_str::<crate::domain::AttachmentMetadata>(
                &row.get::<String, _>("metadata_json"),
            )
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(json_error)?
        .into_iter()
        .map(|a| DeliveredAttachment {
            id: a.id,
            name: a.name,
            media_type: a.media_type,
            size_bytes: a.size_bytes,
            digest_sha256: a.digest_sha256,
        })
        .collect();
        Ok(DeliveryEnvelope {
            schema_version: SchemaVersion::CURRENT,
            delivery_id,
            case_id,
            output_id,
            request_id: request.request_id,
            idempotency_key: DeliveryEnvelope::attempt_key(delivery_id, attempt),
            produced_at: now,
            prompt: serde_json::from_str(&prompt_json).map_err(json_error)?,
            attachments,
            repository: request.repository,
            metadata: request.metadata,
        })
    }

    /// Atomically record acceptance and the terminal delivery transition.
    pub async fn complete_delivery(
        &self,
        case_id: CaseId,
        delivery_id: DeliveryId,
        receipt: &DeliveryReceipt,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let delivery_transition = CaseTransition::DeliverySucceeded { delivery_id };
        let next = transition(state, &delivery_transition).map_err(transition_error)?;
        let row =
            sqlx::query("SELECT attempt,destination_kind FROM deliveries WHERE id=? AND case_id=?")
                .bind(delivery_id.to_string())
                .bind(case_id.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage_error)?
                .ok_or_else(|| AppError::NotFound {
                    kind: "delivery",
                    id: delivery_id.to_string(),
                })?;
        let attempt = row.get::<i64, _>("attempt").max(1) as u32;
        let destination = row.get::<String, _>("destination_kind");
        sqlx::query("UPDATE deliveries SET status='accepted',receipt_json=?,updated_at=? WHERE id=? AND case_id=?").bind(json(receipt)?).bind(timestamp(now)).bind(delivery_id.to_string()).bind(case_id.to_string()).execute(&mut *tx).await.map_err(storage_error)?;
        self.set_state_tx(&mut tx, case_id, state, next, delivery_transition, now)
            .await?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::DeliveryAttempted {
                delivery_id,
                destination,
                attempt,
                outcome: Outcome::Succeeded,
                retry_class: None,
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Record a failed attempt that leaves the case in `delivering`.
    ///
    /// The attempt count already lives on the row; what this adds is the
    /// classification and the message, because those are what a person reads
    /// when deciding whether a retry is worth their time. A `needs_user`
    /// failure means retrying without changing anything will fail identically.
    pub async fn record_delivery_failure(
        &self,
        case_id: CaseId,
        delivery_id: DeliveryId,
        retry_class: RetryClass,
        message: impl Into<String>,
    ) -> Result<()> {
        let message = message.into();
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let case_transition = CaseTransition::DeliveryAttemptFailed {
            delivery_id,
            retry_class,
        };
        let next = transition(state, &case_transition).map_err(transition_error)?;
        let attempt = self
            .delivery_attempt_tx(
                &mut tx,
                case_id,
                delivery_id,
                "in_flight",
                retry_class,
                &message,
                now,
            )
            .await?;
        let destination = self.destination_kind_tx(&mut tx, case_id).await?;
        self.set_state_tx(&mut tx, case_id, state, next, case_transition, now)
            .await?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::DeliveryAttempted {
                delivery_id,
                destination,
                attempt,
                outcome: Outcome::Failed,
                retry_class: Some(retry_class),
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Give up on a delivery, moving the case to `failed`.
    ///
    /// The accepted output is deliberately left untouched. `failed` is not a
    /// terminal wall here: it is the state a person retries a delivery from,
    /// and that is only possible because nothing throws the prompt away.
    pub async fn exhaust_delivery(
        &self,
        case_id: CaseId,
        delivery_id: DeliveryId,
        retry_class: RetryClass,
        message: impl Into<String>,
    ) -> Result<()> {
        let message = message.into();
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let case_transition = CaseTransition::DeliveryExhausted { delivery_id };
        let next = transition(state, &case_transition).map_err(transition_error)?;
        let attempt = self
            .delivery_attempt_tx(
                &mut tx,
                case_id,
                delivery_id,
                "failed",
                retry_class,
                &message,
                now,
            )
            .await?;
        let destination = self.destination_kind_tx(&mut tx, case_id).await?;
        self.set_state_tx(&mut tx, case_id, state, next, case_transition, now)
            .await?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::DeliveryAttempted {
                delivery_id,
                destination,
                attempt,
                outcome: Outcome::Failed,
                retry_class: Some(retry_class),
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Queue another delivery for a case a person is retrying by hand.
    ///
    /// It reuses the operation identity `accept_output` queued the first
    /// delivery under, which is what makes an impatient double-click harmless:
    /// the second request re-queues the same job rather than racing a second
    /// one against it.
    pub async fn request_delivery_retry(&self, case_id: CaseId) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        if state != CaseState::Failed {
            return Err(AppError::Invalid {
                message: format!(
                    "delivery can only be retried on a failed case, and case {case_id} is {}",
                    state.as_str()
                ),
            });
        }
        let has_output: Option<String> = sqlx::query_scalar(
            "SELECT id FROM outputs WHERE case_id=? AND accepted_at IS NOT NULL LIMIT 1",
        )
        .bind(case_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        if has_output.is_none() {
            return Err(AppError::Invalid {
                message: format!(
                    "case {case_id} failed before it produced a prompt, so there is nothing to deliver"
                ),
            });
        }
        self.enqueue_tx(
            &mut tx,
            QueueSpec {
                case_id: Some(case_id),
                kind: "deliver",
                operation_id: format!("case:{case_id}:deliver"),
                payload_json: "{}",
                max_attempts: DEFAULT_MAX_ATTEMPTS,
            },
            now,
        )
        .await?;
        self.append_event(
            &mut tx,
            case_id,
            now,
            CaseEventPayload::Note {
                message: "a delivery retry was requested".into(),
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Advance the in-flight delivery of a case that is already `delivering`.
    async fn continue_delivery_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        case_id: CaseId,
        now: chrono::DateTime<Utc>,
    ) -> Result<DeliveryWork> {
        let row = sqlx::query(
            "SELECT id, attempt FROM deliveries WHERE case_id=? AND status='in_flight' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(case_id.to_string())
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| AppError::NotFound {
            kind: "in-flight delivery",
            id: case_id.to_string(),
        })?;
        let delivery_id: DeliveryId =
            row.get::<String, _>("id")
                .parse()
                .map_err(|error: crate::domain::InvalidId| AppError::Storage {
                    message: error.to_string(),
                    retry: RetryClass::NonRetryable,
                })?;
        let attempt = (row.get::<i64, _>("attempt").max(0) as u32).saturating_add(1);
        let output_id = self.accepted_output_id_tx(tx, case_id).await?;
        let envelope = self
            .delivery_envelope_tx(tx, case_id, delivery_id, output_id, attempt, now)
            .await?;
        sqlx::query("UPDATE deliveries SET attempt=?,idempotency_key=?,updated_at=? WHERE id=?")
            .bind(i64::from(attempt))
            .bind(&envelope.idempotency_key)
            .bind(timestamp(now))
            .bind(delivery_id.to_string())
            .execute(&mut **tx)
            .await
            .map_err(storage_error)?;
        Ok(DeliveryWork {
            delivery_id,
            envelope,
        })
    }

    /// Stamp an attempt's outcome on its row and return the attempt number.
    #[allow(clippy::too_many_arguments)]
    async fn delivery_attempt_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        case_id: CaseId,
        delivery_id: DeliveryId,
        status: &str,
        retry_class: RetryClass,
        message: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<u32> {
        let attempt: Option<i64> =
            sqlx::query_scalar("SELECT attempt FROM deliveries WHERE id=? AND case_id=?")
                .bind(delivery_id.to_string())
                .bind(case_id.to_string())
                .fetch_optional(&mut **tx)
                .await
                .map_err(storage_error)?;
        let attempt = attempt.ok_or_else(|| AppError::NotFound {
            kind: "delivery",
            id: delivery_id.to_string(),
        })?;
        sqlx::query(
            "UPDATE deliveries SET status=?,retry_class=?,error=?,updated_at=? WHERE id=? AND case_id=?",
        )
        .bind(status)
        .bind(retry_class.as_str())
        .bind(crate::diagnostics::redaction::redact(message).into_owned())
        .bind(timestamp(now))
        .bind(delivery_id.to_string())
        .bind(case_id.to_string())
        .execute(&mut **tx)
        .await
        .map_err(storage_error)?;
        Ok(attempt.max(0) as u32)
    }

    async fn destination_kind_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        case_id: CaseId,
    ) -> Result<String> {
        Ok(self
            .case_request_tx(tx, case_id)
            .await?
            .destination
            .kind_str()
            .to_owned())
    }

    async fn accepted_output_id_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        case_id: CaseId,
    ) -> Result<OutputId> {
        let raw: String = sqlx::query_scalar(
            "SELECT id FROM outputs WHERE case_id=? AND accepted_at IS NOT NULL \
             ORDER BY accepted_at DESC LIMIT 1",
        )
        .bind(case_id.to_string())
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| AppError::NotFound {
            kind: "accepted output",
            id: case_id.to_string(),
        })?;
        raw.parse()
            .map_err(|error: crate::domain::InvalidId| AppError::Storage {
                message: error.to_string(),
                retry: RetryClass::NonRetryable,
            })
    }

    async fn case_request_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        case_id: CaseId,
    ) -> Result<RefinementRequest> {
        let raw: String =
            sqlx::query_scalar("SELECT request_json FROM case_inputs WHERE case_id=?")
                .bind(case_id.to_string())
                .fetch_one(&mut **tx)
                .await
                .map_err(storage_error)?;
        serde_json::from_str(&raw).map_err(json_error)
    }
    /// Return the case's live run, creating one when it has none.
    ///
    /// Resume depends on this being idempotent: a case that was interrupted
    /// mid-conversation must come back to the same run rather than start a
    /// second one beside it, or the replayed history would be empty and the
    /// model would be asked to refine a transcript it had already read.
    pub async fn open_run(&self, case_id: CaseId, backend_id: &str) -> Result<BackendRun> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        self.case_state_tx(&mut tx, case_id).await?;
        let existing = sqlx::query(
            "SELECT id,case_id,backend_id,status,step,history_json,usage_json FROM backend_runs WHERE case_id=? AND status='active' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(case_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage_error)?;
        if let Some(row) = existing {
            tx.commit().await.map_err(storage_error)?;
            return backend_run(row);
        }
        let id = RunId::new();
        sqlx::query("INSERT INTO backend_runs (id,case_id,backend_id,status,step,history_json,created_at,updated_at) VALUES (?,?,?,'active',0,'[]',?,?)")
            .bind(id.to_string())
            .bind(case_id.to_string())
            .bind(backend_id)
            .bind(timestamp(now))
            .bind(timestamp(now))
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(BackendRun {
            id,
            case_id,
            backend_id: backend_id.to_owned(),
            status: RunStatus::Active,
            step: 0,
            history: serde_json::Value::Array(Vec::new()),
            usage: None,
        })
    }

    /// Persist the conversation after one provider step, and record the call.
    ///
    /// History and the audit event are written together: a provider call that
    /// is visible in the history but absent from the event log, or the reverse,
    /// would make the audit trail unusable for the one question it exists to
    /// answer — what did this case send to the provider, and when.
    pub async fn save_run_step(
        &self,
        run: &BackendRun,
        history: &serde_json::Value,
        usage: Option<UsageMetadata>,
        outcome: Outcome,
    ) -> Result<u32> {
        let now = Utc::now();
        let step = run.step + 1;
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        sqlx::query(
            "UPDATE backend_runs SET step=?,history_json=?,usage_json=?,updated_at=? WHERE id=?",
        )
        .bind(step as i64)
        .bind(json(history)?)
        .bind(usage.map(|usage| json(&usage)).transpose()?)
        .bind(timestamp(now))
        .bind(run.id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        self.append_event(
            &mut tx,
            run.case_id,
            now,
            CaseEventPayload::ProviderCallCompleted {
                backend_id: run.backend_id.clone(),
                run_id: run.id,
                step,
                usage,
                outcome,
            },
        )
        .await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(step)
    }

    /// Close a run, recording why when it did not finish cleanly.
    pub async fn finish_run(
        &self,
        run_id: RunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();
        sqlx::query(
            "UPDATE backend_runs SET status=?,finished_at=?,updated_at=?,error=? WHERE id=?",
        )
        .bind(status.as_str())
        .bind(timestamp(now))
        .bind(timestamp(now))
        .bind(error)
        .bind(run_id.to_string())
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Move a received case into preparation.
    pub async fn mark_preparation_started(&self, case_id: CaseId) -> Result<()> {
        self.apply_transition(case_id, CaseTransition::PreparationStarted)
            .await
    }

    /// Move a prepared case into the backend's hands.
    pub async fn mark_backend_started(&self, case_id: CaseId) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let next = transition(state, &CaseTransition::BackendStarted).map_err(transition_error)?;
        self.set_state_tx(
            &mut tx,
            case_id,
            state,
            next,
            CaseTransition::BackendStarted,
            now,
        )
        .await?;
        // The queued run job is what makes `running` mean "a worker is on it".
        // Enqueuing it in the same transaction as the transition is what keeps
        // the two from disagreeing: a crash between them would otherwise leave
        // a case that says it is running with nothing scheduled to run it, and
        // only a restart's recovery sweep would notice.
        self.enqueue_tx(
            &mut tx,
            QueueSpec {
                case_id: Some(case_id),
                kind: "run",
                operation_id: format!("case:{case_id}:run"),
                payload_json: "{}",
                max_attempts: DEFAULT_MAX_ATTEMPTS,
            },
            now,
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Accept a validated output and queue its delivery.
    ///
    /// `ready` is a transit state: the delivery job is enqueued in the same
    /// transaction that accepts the output, so there is no window in which a
    /// case is ready with nothing scheduled to act on it.
    pub async fn accept_output(&self, case_id: CaseId, output_id: OutputId) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let next = transition(state, &CaseTransition::ValidationSucceeded { output_id })
            .map_err(transition_error)?;
        let updated = sqlx::query("UPDATE outputs SET accepted_at=? WHERE id=? AND case_id=?")
            .bind(timestamp(now))
            .bind(output_id.to_string())
            .bind(case_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        if updated.rows_affected() != 1 {
            return Err(AppError::NotFound {
                kind: "output",
                id: output_id.to_string(),
            });
        }
        self.set_state_tx(
            &mut tx,
            case_id,
            state,
            next,
            CaseTransition::ValidationSucceeded { output_id },
            now,
        )
        .await?;
        self.enqueue_tx(
            &mut tx,
            QueueSpec {
                case_id: Some(case_id),
                kind: "deliver",
                operation_id: format!("case:{case_id}:deliver"),
                payload_json: "{}",
                max_attempts: DEFAULT_MAX_ATTEMPTS,
            },
            now,
        )
        .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Return a rejected output to the backend for its one corrective retry.
    pub async fn reject_output_for_retry(
        &self,
        case_id: CaseId,
        output_id: OutputId,
    ) -> Result<()> {
        self.apply_transition(
            case_id,
            CaseTransition::ValidationRejectedWithRetry { output_id },
        )
        .await
    }

    /// Fail a case with a person-readable, secret-free reason.
    pub async fn fail_case(&self, case_id: CaseId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let event_transition = CaseTransition::Failed {
            reason: reason.clone(),
        };
        let next = transition(state, &event_transition).map_err(transition_error)?;
        sqlx::query("UPDATE cases SET failure_reason=? WHERE id=?")
            .bind(&reason)
            .bind(case_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        self.withdraw_questions_tx(&mut tx, case_id, now).await?;
        self.set_state_tx(&mut tx, case_id, state, next, event_transition, now)
            .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// How many candidate outputs this case has already had rejected.
    ///
    /// The corrective-retry budget has to survive a restart, so it is counted
    /// from the durable record rather than held in the loop. A case that
    /// crashed between recording a rejection and retrying comes back knowing
    /// it has already spent its retry.
    pub async fn rejected_output_count(&self, case_id: CaseId) -> Result<u32> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM outputs WHERE case_id=? AND valid=0")
                .bind(case_id.to_string())
                .fetch_one(&self.pool)
                .await
                .map_err(storage_error)?;
        Ok(count as u32)
    }

    /// The request exactly as it was submitted.
    pub async fn case_request(&self, case_id: CaseId) -> Result<RefinementRequest> {
        let row = sqlx::query("SELECT request_json FROM case_inputs WHERE case_id=?")
            .bind(case_id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| AppError::NotFound {
                kind: "case",
                id: case_id.to_string(),
            })?;
        serde_json::from_str(&row.get::<String, _>("request_json")).map_err(json_error)
    }

    /// One registered repository by identity.
    pub async fn repository(
        &self,
        id: crate::domain::RepositoryId,
    ) -> Result<crate::repositories::Repository> {
        let row = sqlx::query(
            "SELECT id,canonical_path,policy_json,created_at FROM repositories WHERE id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| AppError::NotFound {
            kind: "repository",
            id: id.to_string(),
        })?;
        Ok(crate::repositories::Repository {
            id,
            root: std::path::PathBuf::from(row.get::<String, _>("canonical_path")),
            policy: serde_json::from_str(row.get("policy_json")).map_err(json_error)?,
            registered_at: super::parse_time(row.get("created_at"))?,
        })
    }

    /// The case's outstanding question request, when it has one.
    ///
    /// At most one can exist: the unique partial index on pending requests is
    /// what enforces "one outstanding question per case", so this returning a
    /// single value is a database guarantee rather than a convention.
    pub async fn pending_question(&self, case_id: CaseId) -> Result<Option<QuestionRequest>> {
        let row = sqlx::query(
            "SELECT request_json,status FROM question_requests WHERE case_id=? AND status='pending'",
        )
        .bind(case_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_error)?;
        row.map(stored_question_request).transpose()
    }

    /// One question request by identity, answered or not.
    pub async fn question_request(
        &self,
        id: crate::domain::QuestionRequestId,
    ) -> Result<QuestionRequest> {
        let row = sqlx::query("SELECT request_json,status FROM question_requests WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| AppError::NotFound {
                kind: "question_request",
                id: id.to_string(),
            })?;
        stored_question_request(row)
    }

    /// Append an observation to a case's history without changing its state.
    ///
    /// The state machine covers what a case *is*; this covers what happened to
    /// it that a person may need to read later — a tool refusal the model
    /// worked around, a submission handed back for correction. Refusing to let
    /// such a thing be recorded because it moves nothing would leave the
    /// history describing only the parts that went to plan.
    pub async fn record_note(&self, case_id: CaseId, payload: CaseEventPayload) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        self.case_state_tx(&mut tx, case_id).await?;
        self.append_event(&mut tx, case_id, now, payload).await?;
        tx.commit().await.map_err(storage_error)
    }

    /// The most recently answered question request and its answers.
    ///
    /// This is what a resuming backend needs: the function call it made before
    /// it stopped, and the result to hand back for it.
    pub async fn latest_answers(&self, case_id: CaseId) -> Result<Option<AnswerSet>> {
        let Some(row) = sqlx::query(
            "SELECT id FROM question_requests WHERE case_id=? AND status='answered' ORDER BY answered_at DESC LIMIT 1",
        )
        .bind(case_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_error)?
        else {
            return Ok(None);
        };
        let question_request_id =
            row.get::<String, _>("id")
                .parse()
                .map_err(|error: crate::domain::InvalidId| AppError::Storage {
                    message: error.to_string(),
                    retry: RetryClass::NonRetryable,
                })?;
        Ok(Some(AnswerSet {
            schema_version: SchemaVersion::CURRENT,
            case_id,
            question_request_id,
            answers: self.answers_for(question_request_id).await?,
        }))
    }

    /// Every answer recorded against one question request, in arrival order.
    pub async fn answers_for(
        &self,
        question_request_id: crate::domain::QuestionRequestId,
    ) -> Result<Vec<Answer>> {
        let rows =
            sqlx::query("SELECT answer_json FROM answers WHERE question_request_id=? ORDER BY id")
                .bind(question_request_id.to_string())
                .fetch_all(&self.pool)
                .await
                .map_err(storage_error)?;
        rows.into_iter()
            .map(|row| serde_json::from_str(&row.get::<String, _>("answer_json")))
            .collect::<std::result::Result<Vec<Answer>, _>>()
            .map_err(json_error)
    }

    /// Apply one transition that carries no extra row writes of its own.
    /// Cancel live work through the frozen state machine.
    ///
    /// An outstanding question is withdrawn in the same transaction. Leaving it
    /// pending would keep offering an answer form for a case that has stopped,
    /// and would hold the one-pending-request slot against a case that can
    /// never use it.
    pub async fn cancel_case(&self, case_id: CaseId, reason: Option<String>) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let event_transition = CaseTransition::Cancelled { reason };
        let next = transition(state, &event_transition).map_err(transition_error)?;
        self.withdraw_questions_tx(&mut tx, case_id, now).await?;
        self.set_state_tx(&mut tx, case_id, state, next, event_transition, now)
            .await?;
        tx.commit().await.map_err(storage_error)
    }

    /// Withdraw any pending question request for a case that has stopped.
    async fn withdraw_questions_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        case_id: CaseId,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE question_requests SET status='cancelled', answered_at=? \
             WHERE case_id=? AND status='pending'",
        )
        .bind(timestamp(now))
        .bind(case_id.to_string())
        .execute(&mut **tx)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    async fn apply_transition(
        &self,
        case_id: CaseId,
        event_transition: CaseTransition,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        let state = self.case_state_tx(&mut tx, case_id).await?;
        let next = transition(state, &event_transition).map_err(transition_error)?;
        self.set_state_tx(&mut tx, case_id, state, next, event_transition, now)
            .await?;
        tx.commit().await.map_err(storage_error)
    }
}

/// Read a question request, taking its status from the column.
///
/// `request_json` is the document as it was raised, and its `status` field is
/// a snapshot of that moment. The column is what later writes update, so the
/// column is the authority and the document is stamped with it on the way out.
/// One authority, and no update that has to remember to rewrite a blob.
pub(super) fn stored_question_request(row: sqlx::sqlite::SqliteRow) -> Result<QuestionRequest> {
    let mut request: QuestionRequest =
        serde_json::from_str(&row.get::<String, _>("request_json")).map_err(json_error)?;
    request.status = match row.get::<String, _>("status").as_str() {
        "pending" => crate::domain::QuestionRequestStatus::Pending,
        "answered" => crate::domain::QuestionRequestStatus::Answered,
        "cancelled" => crate::domain::QuestionRequestStatus::Cancelled,
        other => {
            return Err(AppError::Storage {
                message: format!("invalid stored question request status {other}"),
                retry: RetryClass::NonRetryable,
            })
        }
    };
    Ok(request)
}

fn backend_run(row: sqlx::sqlite::SqliteRow) -> Result<BackendRun> {
    Ok(BackendRun {
        id: row
            .get::<String, _>("id")
            .parse()
            .map_err(|error: crate::domain::InvalidId| AppError::Storage {
                message: error.to_string(),
                retry: RetryClass::NonRetryable,
            })?,
        case_id: parse_case_id(row.get("case_id"))?,
        backend_id: row.get("backend_id"),
        status: RunStatus::parse(&row.get::<String, _>("status"))?,
        step: row.get::<i64, _>("step") as u32,
        history: serde_json::from_str(&row.get::<String, _>("history_json")).map_err(json_error)?,
        usage: row
            .get::<Option<String>, _>("usage_json")
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(json_error)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{OutputId, RefinedPrompt};
    use crate::storage::CreateCaseResult;

    async fn running_case() -> (Storage, CaseId) {
        let path = tempfile::tempdir().unwrap().keep().join("runs.db");
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

    fn prompt(criteria: Vec<&str>) -> RefinedPrompt {
        RefinedPrompt {
            schema_version: SchemaVersion::CURRENT,
            title: "Title".into(),
            prompt: "Do the work described here.".into(),
            objective: "The work is done.".into(),
            context: Vec::new(),
            requirements: Vec::new(),
            constraints: Vec::new(),
            acceptance_criteria: criteria.into_iter().map(str::to_owned).collect(),
            references: Vec::new(),
            assumptions: Vec::new(),
            unresolved_questions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_run_is_reopened_rather_than_started_again() {
        let (storage, case_id) = running_case().await;
        let first = storage.open_run(case_id, "gemini").await.unwrap();
        let history = serde_json::json!([{ "role": "user", "parts": [{ "text": "brief" }] }]);
        let step = storage
            .save_run_step(&first, &history, None, Outcome::Succeeded)
            .await
            .unwrap();
        assert_eq!(step, 1);

        // What a restart sees: the same run, at the step it reached, with the
        // conversation intact.
        let resumed = storage.open_run(case_id, "gemini").await.unwrap();
        assert_eq!(resumed.id, first.id);
        assert_eq!(resumed.step, 1);
        assert_eq!(resumed.history, history);

        storage
            .finish_run(first.id, RunStatus::Finished, None)
            .await
            .unwrap();
        let after = storage.open_run(case_id, "gemini").await.unwrap();
        assert_ne!(after.id, first.id, "a finished run is not reopened");
    }

    #[tokio::test]
    async fn the_corrective_retry_budget_is_counted_from_the_durable_record() {
        let (storage, case_id) = running_case().await;
        assert_eq!(storage.rejected_output_count(case_id).await.unwrap(), 0);

        let rejected = OutputId::new();
        storage
            .record_output(
                case_id,
                rejected,
                &prompt(vec![]),
                false,
                &["acceptance_criteria: state at least 1 condition".to_owned()],
            )
            .await
            .unwrap();
        storage
            .reject_output_for_retry(case_id, rejected)
            .await
            .unwrap();
        assert_eq!(storage.rejected_output_count(case_id).await.unwrap(), 1);
        assert_eq!(
            storage.case_state(case_id).await.unwrap(),
            crate::domain::CaseState::Running,
            "a rejected output returns the case to the backend"
        );

        let accepted = OutputId::new();
        storage
            .record_output(case_id, accepted, &prompt(vec!["it works"]), true, &[])
            .await
            .unwrap();
        storage.accept_output(case_id, accepted).await.unwrap();
        assert_eq!(
            storage.case_state(case_id).await.unwrap(),
            crate::domain::CaseState::Ready
        );
    }

    #[tokio::test]
    async fn accepting_an_output_queues_its_delivery_in_the_same_breath() {
        let (storage, case_id) = running_case().await;
        let output_id = OutputId::new();
        storage
            .record_output(case_id, output_id, &prompt(vec!["it works"]), true, &[])
            .await
            .unwrap();
        storage.accept_output(case_id, output_id).await.unwrap();

        let queued: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM jobs WHERE case_id=? AND status='queued' ORDER BY kind",
        )
        .bind(case_id.to_string())
        .fetch_all(&storage.pool)
        .await
        .unwrap();
        // `prepare` is the job ingress queued and `run` is the one starting the
        // backend queued; nothing has actually run in this test. What matters
        // is that `deliver` joined them inside the accepting transaction, so
        // `ready` is never a state with no work scheduled behind it.
        assert_eq!(queued, vec!["deliver", "prepare", "run"]);
    }

    #[tokio::test]
    async fn a_delivery_is_recorded_before_send_and_completed_once() {
        let (storage, case_id) = running_case().await;
        let output_id = OutputId::new();
        storage
            .record_output(case_id, output_id, &prompt(vec!["it works"]), true, &[])
            .await
            .unwrap();
        storage.accept_output(case_id, output_id).await.unwrap();
        let work = storage.begin_delivery(case_id).await.unwrap();
        assert_eq!(
            storage.case_state(case_id).await.unwrap(),
            crate::domain::CaseState::Delivering
        );
        assert!(work.envelope.idempotency_key.starts_with("refinery:"));
        storage
            .complete_delivery(
                case_id,
                work.delivery_id,
                &DeliveryReceipt {
                    schema_version: SchemaVersion::CURRENT,
                    accepted: true,
                    destination_reference: Some("coo:1".into()),
                    received_at: Some(Utc::now()),
                    message: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            storage.case_state(case_id).await.unwrap(),
            crate::domain::CaseState::Completed
        );
    }

    #[tokio::test]
    async fn failing_a_case_records_the_reason_where_a_person_can_read_it() {
        let (storage, case_id) = running_case().await;
        storage
            .fail_case(case_id, "the provider refused the credential")
            .await
            .unwrap();
        assert_eq!(
            storage.case_state(case_id).await.unwrap(),
            crate::domain::CaseState::Failed
        );
        let reason: Option<String> =
            sqlx::query_scalar("SELECT failure_reason FROM cases WHERE id=?")
                .bind(case_id.to_string())
                .fetch_one(&storage.pool)
                .await
                .unwrap();
        assert_eq!(
            reason.as_deref(),
            Some("the provider refused the credential")
        );
    }
}
