//! The leased durable job runner.
//!
//! Handlers never receive an unleased job: [`JobRunner`] claims it from
//! [`crate::storage::Storage`], owns a heartbeat task for its duration, and
//! completes or requeues it according to the returned [`crate::AppError`]'s
//! retry class. The storage query excludes an already leased case, preserving
//! per-case serialization while independent cases remain claimable.

use std::{future::Future, time::Duration as StdDuration};

use chrono::{Duration, Utc};
use tokio::{sync::watch, task::JoinHandle};

use crate::{
    error::Result,
    storage::{ClaimedJob, Storage},
};

/// A worker configuration with intentionally conservative defaults.
#[derive(Debug, Clone)]
pub struct JobRunnerConfig {
    /// How long a claim remains valid without a heartbeat.
    pub lease_for: Duration,
    /// How often a running handler renews its lease.
    pub heartbeat_every: StdDuration,
}

impl Default for JobRunnerConfig {
    fn default() -> Self {
        Self {
            lease_for: Duration::seconds(30),
            heartbeat_every: StdDuration::from_secs(10),
        }
    }
}

/// A durable runner bound to one worker identity.
#[derive(Clone, Debug)]
pub struct JobRunner {
    store: Storage,
    worker_id: String,
    config: JobRunnerConfig,
}

impl JobRunner {
    /// Construct a worker runner. The worker id must be unique per process.
    pub fn new(store: Storage, worker_id: impl Into<String>, config: JobRunnerConfig) -> Self {
        Self {
            store,
            worker_id: worker_id.into(),
            config,
        }
    }

    /// Claim and run at most one job, returning whether any work was available.
    pub async fn run_once<F, Fut>(&self, handler: F) -> Result<bool>
    where
        F: FnOnce(ClaimedJob) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let Some(job) = self
            .store
            .claim_job(&self.worker_id, self.config.lease_for, Utc::now())
            .await?
        else {
            return Ok(false);
        };
        let (stop, heartbeat) = self.start_heartbeat(job.clone());
        let result = handler(job.clone()).await;
        let _ = stop.send(true);
        let _ = heartbeat.await;
        match result {
            Ok(()) => {
                self.store.complete_job(&job, Utc::now()).await?;
            }
            Err(error) => {
                self.store.fail_job(&job, &error, Utc::now()).await?;
            }
        }
        Ok(true)
    }

    fn start_heartbeat(&self, job: ClaimedJob) -> (watch::Sender<bool>, JoinHandle<()>) {
        let store = self.store.clone();
        let lease_for = self.config.lease_for;
        let every = self.config.heartbeat_every;
        let (stop, mut stopped) = watch::channel(false);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {
                        match store.heartbeat_job(&job, lease_for, Utc::now()).await {
                            Ok(true) => {},
                            _ => break,
                        }
                    }
                    changed = stopped.changed() => {
                        if changed.is_err() || *stopped.borrow() { break; }
                    }
                }
            }
        });
        (stop, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::RefinementRequest, storage::CreateCaseResult};

    async fn store() -> Storage {
        let path = tempfile::tempdir().unwrap().keep().join("runner.db");
        Storage::open(path).await.unwrap()
    }
    fn request() -> RefinementRequest {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/refinement_request.json"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn successful_handler_completes_its_job() {
        let store = store().await;
        let case = match store.create_case_idempotent(&request()).await.unwrap() {
            CreateCaseResult::Created(id) => id,
            CreateCaseResult::Existing(_) => unreachable!(),
        };
        let runner = JobRunner::new(
            store.clone(),
            "worker",
            JobRunnerConfig {
                lease_for: Duration::seconds(2),
                heartbeat_every: StdDuration::from_millis(1),
            },
        );
        assert!(runner
            .run_once(|job| async move {
                assert_eq!(job.case_id, Some(case));
                Ok(())
            })
            .await
            .unwrap());
        assert!(!runner.run_once(|_| async { Ok(()) }).await.unwrap());
    }
}
