//! The daemon's work loop: what actually moves a case through its lifecycle.
//!
//! Everything before this module is a piece — a state machine, a leased job
//! table, an agent backend, a destination adapter. This is where they are
//! joined, and it is deliberately the only place that knows the order. A case
//! is prepared, then run, then delivered, and each of those is a separate
//! durable job so a crash between two of them costs at most one attempt.
//!
//! Three rules shape the handlers:
//!
//! *Nothing long-running happens inside an HTTP request.* Ingress writes a
//! case and queues `prepare`; the browser's answer form writes answers and
//! queues `resume`. The request returns immediately, and this loop picks the
//! work up whenever it next claims a job — a second later or after a restart.
//!
//! *A handler returns the error rather than swallowing it.* The job runner
//! reads its retry classification to decide between another attempt with
//! backoff and giving up, so a handler that turned a failure into `Ok(())`
//! would silently strand a case. What a handler must do before returning an
//! error is make the case's snapshot say the same thing the job table does.
//!
//! *The case is the serialization key.* Storage refuses to lease two jobs for
//! one case at once, so a handler never has to defend against a second worker
//! being inside the same case, and independent cases still run concurrently.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Utc;

use crate::{
    agent::{AgentBackend, BackendContext, GeminiBackend, MediaContext, RunOutcome},
    config::{
        credentials::{CredentialKey, CredentialStore},
        Config,
    },
    domain::{CaseId, Destination},
    error::{AppError, Result, RetryClass},
    integrations::{DestinationAdapter, LocalExportAdapter, LocalOverlordAdapter},
    jobs::{JobRunner, JobRunnerConfig},
    media::{files_api::GeminiFilesClient, lifecycle, store::MediaStore},
    storage::{ClaimedJob, Storage},
};

/// How long the loop sleeps when it finds no claimable job.
///
/// Short enough that a person clicking "retry delivery" sees something happen,
/// long enough that an idle daemon is not a busy loop against SQLite.
const IDLE_POLL: StdDuration = StdDuration::from_millis(250);

/// The daemon's job loop, bound to one installation.
#[derive(Clone)]
pub struct Worker {
    store: Storage,
    config: Arc<Config>,
    runner: JobRunner,
    provider_api_base: String,
    files_api_base: String,
}

impl Worker {
    /// Build the loop for one configuration and store.
    pub fn new(config: Arc<Config>, store: Storage) -> Self {
        let worker_id = format!("{}:{}", hostname(), std::process::id());
        Self {
            runner: JobRunner::new(store.clone(), worker_id, JobRunnerConfig::default()),
            store,
            config,
            provider_api_base: crate::agent::GEMINI_API_BASE.to_owned(),
            files_api_base: crate::agent::GEMINI_API_BASE.to_owned(),
        }
    }

    /// Point the provider clients at a local test double.
    pub fn with_provider_base(mut self, base: impl Into<String>) -> Self {
        let base = base.into();
        self.files_api_base.clone_from(&base);
        self.provider_api_base = base;
        self
    }

    /// Claim and run at most one job. Returns whether any work was available.
    ///
    /// Exposed so a test can drive the loop one step at a time instead of
    /// racing a background task.
    pub async fn step(&self) -> Result<bool> {
        let worker = self.clone();
        self.runner
            .run_once(move |job| async move { worker.handle(job).await })
            .await
    }

    /// Run until the process is asked to stop.
    pub async fn run_until(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            match self.step().await {
                // Work was found and handled; look for more immediately.
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    // A failure to claim is a storage problem, not a case
                    // problem: log it and back off rather than spin.
                    tracing::error!(error = %error, "the job loop could not claim work");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(IDLE_POLL) => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    async fn handle(&self, job: ClaimedJob) -> Result<()> {
        let Some(case_id) = job.case_id else {
            return Err(AppError::Invalid {
                message: format!("job {} has no case to work on", job.operation_id),
            });
        };
        tracing::info!(
            case_id = %case_id,
            kind = %job.kind,
            attempt = job.attempts,
            "handling job"
        );
        match job.kind.as_str() {
            "prepare" => self.prepare(case_id).await,
            "run" => self.run_backend(case_id).await,
            "deliver" => self.deliver(&job, case_id).await,
            other => Err(AppError::Invalid {
                message: format!("unknown job kind {other}"),
            }),
        }
    }

    /// Import attachments, make provider copies available, and hand the case
    /// to the backend.
    ///
    /// Preparation is separate from the run because it is the part that can
    /// fail for reasons the user can fix — an unreadable file, an expired
    /// upload — and separating it means a retry re-imports rather than
    /// re-running a provider conversation that already cost tokens.
    async fn prepare(&self, case_id: CaseId) -> Result<()> {
        self.store.mark_preparation_started(case_id).await?;
        let request = self.store.case_request(case_id).await?;
        let media = self.media_context()?;
        if let Some(media) = &media {
            lifecycle::import_request_attachments(
                &self.store,
                &media.store,
                case_id,
                &request.attachments,
            )
            .await?;
            lifecycle::ensure_case_attachments_available(
                &self.store,
                &media.store,
                &media.files,
                case_id,
                Utc::now(),
            )
            .await?;
        } else if !request.attachments.is_empty() {
            return Err(AppError::NeedsUser {
                message: "this case has attachments but no provider credential is configured to \
                          upload them; run `refinery provider configure gemini`"
                    .into(),
            });
        }
        self.store.mark_backend_started(case_id).await?;
        Ok(())
    }

    /// Drive the agent backend for one turn of the case.
    ///
    /// The backend decides for itself whether this is a first run or a resume
    /// by reading the persisted conversation, so this handler does not have to
    /// tell them apart — which matters, because after a restart it could not.
    async fn run_backend(&self, case_id: CaseId) -> Result<()> {
        let backend = self.backend()?;
        let context =
            BackendContext::load(self.store.clone(), case_id, self.media_context()?).await?;
        match backend.start(&context).await? {
            RunOutcome::Completed { output_id } => {
                tracing::info!(case_id = %case_id, output_id = %output_id, "refinement accepted");
            }
            RunOutcome::AwaitingAnswer {
                question_request_id,
            } => {
                tracing::info!(
                    case_id = %case_id,
                    question_request_id = %question_request_id,
                    "case is waiting for an answer"
                );
            }
            RunOutcome::Failed { reason } => {
                tracing::warn!(case_id = %case_id, reason = %reason, "refinement failed");
            }
        }
        Ok(())
    }

    /// Attempt one delivery and record what happened.
    ///
    /// The two failure paths are the whole point of this handler. While
    /// attempts remain and the failure is retryable, the attempt is recorded
    /// and the case stays in `delivering` — the job runner schedules the next
    /// try with backoff. When this was the last attempt, or the failure is one
    /// no retry can fix, the delivery is exhausted and the case moves to
    /// `failed` with its prompt preserved, which is the state the interface
    /// offers a person the retry button from.
    async fn deliver(&self, job: &ClaimedJob, case_id: CaseId) -> Result<()> {
        let request = self.store.case_request(case_id).await?;
        let adapter = destination_adapter(&request.destination);
        let work = self.store.begin_delivery(case_id).await?;
        match adapter.submit(&work.envelope).await {
            Ok(receipt) if receipt.accepted => {
                self.store
                    .complete_delivery(case_id, work.delivery_id, &receipt)
                    .await
            }
            Ok(receipt) => {
                // A receipt that says "not accepted" is a refusal, not a
                // transport failure: retrying the identical envelope would be
                // refused identically.
                let message = receipt
                    .message
                    .unwrap_or_else(|| "the destination refused the prompt".into());
                self.store
                    .exhaust_delivery(
                        case_id,
                        work.delivery_id,
                        RetryClass::NonRetryable,
                        message.clone(),
                    )
                    .await?;
                Err(AppError::Upstream {
                    service: "destination",
                    message,
                    retry: RetryClass::NonRetryable,
                })
            }
            Err(error) => {
                let retry_class = error.retry_class();
                let message = crate::diagnostics::redaction::redact_detail(&error);
                if retry_class.is_retryable() && job.attempts < job.max_attempts {
                    self.store
                        .record_delivery_failure(case_id, work.delivery_id, retry_class, message)
                        .await?;
                } else {
                    self.store
                        .exhaust_delivery(case_id, work.delivery_id, retry_class, message)
                        .await?;
                }
                Err(error)
            }
        }
    }

    fn backend(&self) -> Result<GeminiBackend> {
        let api_key = self
            .provider_api_key()?
            .ok_or_else(|| AppError::NeedsUser {
                message:
                    "no Gemini API key is configured; run `refinery provider configure gemini`"
                        .into(),
            })?;
        GeminiBackend::with_base_url(
            &self.provider_api_base,
            &self.config.settings.provider.model,
            api_key,
        )
    }

    /// The media handles, or `None` when no provider credential is stored.
    ///
    /// A case with no attachments does not need a provider file store, and an
    /// installation that has not been given a key yet should still be able to
    /// serve its interface rather than refusing to start.
    fn media_context(&self) -> Result<Option<MediaContext>> {
        let Some(api_key) = self.provider_api_key()? else {
            return Ok(None);
        };
        Ok(Some(MediaContext {
            store: MediaStore::from_config(&self.config),
            files: GeminiFilesClient::new(&self.files_api_base, api_key)?,
        }))
    }

    fn provider_api_key(&self) -> Result<Option<crate::domain::SecretString>> {
        CredentialStore::open(&self.config.data_dir.credentials_dir()).get(
            &CredentialKey::provider_api_key(&self.config.settings.provider.backend),
        )
    }
}

/// Choose the adapter the request's destination names.
///
/// A destination is data supplied by whoever submitted the case, so this is a
/// closed match over the two kinds Refinery ships rather than anything the
/// submitter can extend. Both are loopback or local by contract validation.
fn destination_adapter(destination: &Destination) -> Box<dyn DestinationAdapter> {
    match destination {
        Destination::Overlord(overlord) => Box::new(LocalOverlordAdapter::new(
            overlord.base_url.clone(),
            overlord.bearer_token.clone(),
        )),
        Destination::LocalExport(export) => Box::new(LocalExportAdapter::new(&export.path)),
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "refinery".into())
}
