//! The desktop window's connection to the daemon.
//!
//! The window is a client of the same loopback API the browser interface and
//! Overlord use, authenticated with the same locally generated token. When no
//! daemon answers on the configured port, the window hosts one itself on a
//! background runtime: the same router, the same job loop, the same shutdown
//! ordering as `refinery serve`. A person who has installed the background
//! service sees the window attach to it; a person who has not sees the window
//! simply work.
//!
//! Every call runs off the interface thread and reports back through a
//! channel as a [`Msg`], so the window never blocks on the network, the
//! database, or the operating-system credential store.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use eframe::egui;
use serde::{de::DeserializeOwned, Deserialize};

use crate::{
    agent,
    config::{
        credentials::{CredentialKey, CredentialStore},
        Config,
    },
    domain::{AnswerSet, ApiError, CaseId, RefinementRequest, RepositoryId, SecretString},
    error::{AppError, Result},
    storage::{CaseDetail, CaseSummary, Storage},
};

/// How long the window waits for a daemon it started to answer.
const EMBEDDED_START_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-request ceiling for API calls from the window.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Who the window identifies as when it submits a case or an answer.
pub const INSTANCE: &str = "refinery-desktop";

/// What the health route reports, as much of it as the window shows.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Health {
    /// `ok` or `degraded`.
    #[serde(default)]
    pub status: String,
    /// The daemon's version.
    #[serde(default)]
    pub version: String,
    /// The daemon's data directory.
    #[serde(default)]
    pub data_dir: String,
    /// The configured model identifier.
    #[serde(default)]
    pub model: String,
    /// The configured provider backend.
    #[serde(default)]
    pub provider: String,
    /// Whether a provider credential is stored.
    #[serde(default)]
    pub provider_credential: bool,
    /// Cases by lifecycle state.
    #[serde(default)]
    pub cases_by_state: BTreeMap<String, u64>,
    /// Problems the integrity check found.
    #[serde(default)]
    pub integrity_problems: Vec<String>,
}

/// One registered repository, as the API lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct RepositoryView {
    /// Durable identity.
    pub id: RepositoryId,
    /// The canonical root.
    pub root: PathBuf,
    /// The directory's own name.
    #[serde(default)]
    pub label: String,
    /// Whether the root is a Git work tree.
    #[serde(default)]
    pub is_git_work_tree: bool,
}

/// The result of storing a provider key.
#[derive(Debug, Clone)]
pub struct KeyOutcome {
    /// Where the key was stored.
    pub backend: &'static str,
    /// The connection test: a model summary, or why it failed.
    pub verified: std::result::Result<String, String>,
}

/// A completed background call, delivered to the window.
#[derive(Debug)]
pub enum Msg {
    /// The health route answered, or did not.
    Health(std::result::Result<Health, String>),
    /// The repository list.
    Repositories(std::result::Result<Vec<RepositoryView>, String>),
    /// A repository was registered.
    RepositoryAdded(std::result::Result<RepositoryView, String>),
    /// A repository was forgotten.
    RepositoryForgotten(std::result::Result<RepositoryId, String>),
    /// The case list.
    Cases(std::result::Result<Vec<CaseSummary>, String>),
    /// One case's detail.
    Case(std::result::Result<Box<CaseDetail>, String>),
    /// A case was submitted.
    Submitted(std::result::Result<CaseId, String>),
    /// Answers were recorded.
    Answered(std::result::Result<CaseId, String>),
    /// A case was cancelled.
    Cancelled(std::result::Result<CaseId, String>),
    /// A delivery retry was requested.
    RetryRequested(std::result::Result<CaseId, String>),
    /// A provider key was stored and tested.
    KeyStored(std::result::Result<KeyOutcome, String>),
    /// The stored provider key was tested.
    KeyTested(std::result::Result<String, String>),
    /// The provider key was removed.
    KeyRemoved(std::result::Result<(), String>),
}

/// How the window reached a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMode {
    /// The window started the daemon on its own runtime.
    Embedded,
    /// A daemon was already listening, for example the installed service.
    External,
}

/// Wakes the window when a message lands. Attached once the window exists.
#[derive(Clone, Default)]
pub struct Notifier(Arc<Mutex<Option<egui::Context>>>);

impl Notifier {
    /// Remember the window to wake.
    pub fn attach(&self, ctx: egui::Context) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ctx);
    }

    fn wake(&self) {
        if let Some(ctx) = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            ctx.request_repaint();
        }
    }
}

/// The window's handle on the daemon.
pub struct Backend {
    config: Arc<Config>,
    runtime: tokio::runtime::Runtime,
    http: reqwest::Client,
    base: String,
    token: Arc<str>,
    tx: mpsc::Sender<Msg>,
    notifier: Notifier,
    mode: DaemonMode,
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
}

impl Backend {
    /// Reach the daemon, starting one when none answers.
    ///
    /// Returns the backend and the receiving end of its message channel.
    pub fn start(config: Config, notifier: Notifier) -> Result<(Self, mpsc::Receiver<Msg>)> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("refinery-desktop")
            .build()
            .map_err(|source| AppError::io("tokio runtime", source))?;
        let token: Arc<str> = Arc::from(crate::api::ensure_token(&config)?);
        crate::diagnostics::redaction::register_secret(&token);
        let address = config.settings.api.bind_address();
        let base = format!("http://{address}");
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|source| AppError::Internal(anyhow::Error::new(source)))?;
        let (tx, rx) = mpsc::channel();
        let config = Arc::new(config);

        let probe = runtime.block_on(probe(&http, &base, &token));
        let (mode, shutdown) = match probe {
            Probe::Running => (DaemonMode::External, None),
            Probe::Unauthorized => {
                return Err(AppError::NeedsUser {
                    message: format!(
                        "a Refinery daemon is listening on {address} but refuses this window's token; \
                         it is probably serving a different data directory. Stop it with \
                         `refinery service stop`, or point this window at the same data directory."
                    ),
                });
            }
            Probe::Unreachable => {
                let signal = runtime.block_on(start_embedded(Arc::clone(&config)))?;
                runtime.block_on(wait_until_healthy(&http, &base, &token))?;
                (DaemonMode::Embedded, Some(signal))
            }
        };
        tracing::info!(?mode, %address, "desktop window reached the daemon");

        Ok((
            Self {
                config,
                runtime,
                http,
                base,
                token,
                tx,
                notifier,
                mode,
                shutdown,
            },
            rx,
        ))
    }

    /// The daemon's configuration as this window loaded it.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether the window hosts the daemon or found one running.
    pub fn mode(&self) -> &DaemonMode {
        &self.mode
    }

    /// The interface URL carrying the token, for handing off to a browser.
    pub fn interface_url(&self) -> String {
        format!(
            "{}/?token={}",
            self.base,
            url::form_urlencoded::byte_serialize(self.token.as_bytes()).collect::<String>()
        )
    }

    /// Where local exports land when the window keeps a result on disk.
    pub fn export_dir(&self) -> PathBuf {
        self.config.data_dir.root().join("exports")
    }

    fn client(&self) -> Client {
        Client {
            http: self.http.clone(),
            base: self.base.clone(),
            token: Arc::clone(&self.token),
        }
    }

    fn spawn<F>(&self, work: F)
    where
        F: std::future::Future<Output = Msg> + Send + 'static,
    {
        let tx = self.tx.clone();
        let notifier = self.notifier.clone();
        self.runtime.spawn(async move {
            let message = work.await;
            let _ = tx.send(message);
            notifier.wake();
        });
    }

    /// Ask the health route.
    pub fn refresh_health(&self) {
        let client = self.client();
        self.spawn(async move { Msg::Health(client.get("/v1/health").await) });
    }

    /// List registered repositories.
    pub fn refresh_repositories(&self) {
        let client = self.client();
        self.spawn(async move { Msg::Repositories(client.get("/v1/repositories").await) });
    }

    /// Register a directory as a repository.
    pub fn add_repository(&self, path: PathBuf) {
        let client = self.client();
        self.spawn(async move {
            let body = serde_json::json!({ "path": path.display().to_string() });
            Msg::RepositoryAdded(client.post("/v1/repositories", Some(body)).await)
        });
    }

    /// Forget a repository registration.
    pub fn forget_repository(&self, id: RepositoryId) {
        let client = self.client();
        self.spawn(async move {
            let result: std::result::Result<serde_json::Value, String> =
                client.delete(&format!("/v1/repositories/{id}")).await;
            Msg::RepositoryForgotten(result.map(|_| id))
        });
    }

    /// List recent cases.
    pub fn refresh_cases(&self) {
        let client = self.client();
        self.spawn(async move { Msg::Cases(client.get("/v1/refinements?limit=100").await) });
    }

    /// Load one case in full.
    pub fn load_case(&self, id: CaseId) {
        let client = self.client();
        self.spawn(async move {
            let result: std::result::Result<CaseDetail, String> =
                client.get(&format!("/v1/refinements/{id}")).await;
            Msg::Case(result.map(Box::new))
        });
    }

    /// Submit a refinement request.
    pub fn submit(&self, request: RefinementRequest) {
        let client = self.client();
        self.spawn(async move {
            #[derive(Deserialize)]
            struct Accepted {
                case_id: CaseId,
            }
            let body = match serde_json::to_value(&request) {
                Ok(body) => body,
                Err(error) => return Msg::Submitted(Err(error.to_string())),
            };
            let result: std::result::Result<Accepted, String> =
                client.post("/v1/refinements", Some(body)).await;
            Msg::Submitted(result.map(|accepted| accepted.case_id))
        });
    }

    /// Record answers to a case's pending question.
    pub fn answer(&self, answers: AnswerSet) {
        let client = self.client();
        self.spawn(async move {
            let case_id = answers.case_id;
            let body = match serde_json::to_value(&answers) {
                Ok(body) => body,
                Err(error) => return Msg::Answered(Err(error.to_string())),
            };
            let result = client
                .post_empty(&format!("/v1/refinements/{case_id}/answers"), Some(body))
                .await;
            Msg::Answered(result.map(|()| case_id))
        });
    }

    /// Cancel a case.
    pub fn cancel(&self, id: CaseId) {
        let client = self.client();
        self.spawn(async move {
            let result = client
                .post_empty(&format!("/v1/refinements/{id}/cancel"), None)
                .await;
            Msg::Cancelled(result.map(|()| id))
        });
    }

    /// Retry a case's delivery.
    pub fn retry_delivery(&self, id: CaseId) {
        let client = self.client();
        self.spawn(async move {
            let result = client
                .post_empty(&format!("/v1/refinements/{id}/deliveries"), None)
                .await;
            Msg::RetryRequested(result.map(|()| id))
        });
    }

    /// Store a provider API key, then test it against the provider.
    ///
    /// The secret goes straight to the credential store on a blocking thread;
    /// it is never logged, and the message that comes back describes the
    /// model, not the key.
    pub fn store_api_key(&self, key: String) {
        let config = Arc::clone(&self.config);
        let tx = self.tx.clone();
        let notifier = self.notifier.clone();
        self.runtime.spawn(async move {
            let secret = SecretString::new(key.trim());
            let store_config = Arc::clone(&config);
            let stored = tokio::task::spawn_blocking(move || {
                let store = CredentialStore::open(&store_config.data_dir.credentials_dir());
                store
                    .set(
                        &CredentialKey::provider_api_key(&store_config.settings.provider.backend),
                        &secret,
                    )
                    .map(|()| (store.backend().label(), secret))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
            let message = match stored {
                Err(error) => Msg::KeyStored(Err(error)),
                Ok((backend, secret)) => {
                    let verified = test_key(&config, &secret).await;
                    Msg::KeyStored(Ok(KeyOutcome { backend, verified }))
                }
            };
            let _ = tx.send(message);
            notifier.wake();
        });
    }

    /// Test the stored provider key without showing it.
    pub fn test_api_key(&self) {
        let config = Arc::clone(&self.config);
        self.spawn(async move {
            let read_config = Arc::clone(&config);
            let secret = tokio::task::spawn_blocking(move || {
                CredentialStore::open(&read_config.data_dir.credentials_dir()).get(
                    &CredentialKey::provider_api_key(&read_config.settings.provider.backend),
                )
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
            match secret {
                Err(error) => Msg::KeyTested(Err(error)),
                Ok(None) => Msg::KeyTested(Err("no API key is stored".into())),
                Ok(Some(secret)) => Msg::KeyTested(test_key(&config, &secret).await),
            }
        });
    }

    /// Remove the stored provider key.
    pub fn remove_api_key(&self) {
        let config = Arc::clone(&self.config);
        self.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                CredentialStore::open(&config.data_dir.credentials_dir()).delete(
                    &CredentialKey::provider_api_key(&config.settings.provider.backend),
                )
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
            Msg::KeyRemoved(result)
        });
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(signal) = self.shutdown.take() {
            let _ = signal.send(true);
        }
        // Give the daemon a moment to finish in-flight work; the runtime is
        // torn down when the window closes regardless.
        let runtime = std::mem::replace(
            &mut self.runtime,
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a bare runtime builds"),
        );
        runtime.shutdown_timeout(Duration::from_secs(3));
    }
}

async fn test_key(config: &Config, secret: &SecretString) -> std::result::Result<String, String> {
    if config.settings.provider.backend != "gemini" {
        return Err(format!(
            "no connection test exists for provider `{}`",
            config.settings.provider.backend
        ));
    }
    agent::test_gemini(
        agent::GEMINI_API_BASE,
        &config.settings.provider.model,
        secret,
    )
    .await
    .map(|connection| connection.summary())
    .map_err(|error| error.to_string())
}

enum Probe {
    Running,
    Unauthorized,
    Unreachable,
}

async fn probe(http: &reqwest::Client, base: &str, token: &str) -> Probe {
    match http
        .get(format!("{base}/v1/health"))
        .bearer_auth(token)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
            Probe::Unauthorized
        }
        Ok(_) => Probe::Running,
        Err(_) => Probe::Unreachable,
    }
}

/// Start the daemon on the current runtime and return its shutdown signal.
async fn start_embedded(config: Arc<Config>) -> Result<tokio::sync::watch::Sender<bool>> {
    let store = Storage::open(config.data_dir.database_file()).await?;
    let resumed = store.recover_startup(chrono::Utc::now()).await?;
    tracing::debug!(
        resumed_cases = resumed,
        "desktop window recovered durable state"
    );
    let address = config.settings.api.bind_address();
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| AppError::Io {
            path: PathBuf::from(address.to_string()),
            source,
        })?;
    let (signal, shutdown) = tokio::sync::watch::channel(false);
    let worker = crate::cases::Worker::new(Arc::clone(&config), store.clone());
    tokio::spawn(async move {
        if let Err(error) = crate::api::serve_on(listener, &config, store, worker, shutdown).await {
            tracing::error!(%error, "embedded daemon stopped");
        }
    });
    Ok(signal)
}

async fn wait_until_healthy(http: &reqwest::Client, base: &str, token: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + EMBEDDED_START_TIMEOUT;
    loop {
        if matches!(probe(http, base, token).await, Probe::Running) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::Internal(anyhow::anyhow!(
                "the embedded daemon did not answer on {base} within {:?}",
                EMBEDDED_START_TIMEOUT
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A cheap, cloneable view of the HTTP transport for one call.
struct Client {
    http: reqwest::Client,
    base: String,
    token: Arc<str>,
}

impl Client {
    async fn get<T: DeserializeOwned>(&self, path: &str) -> std::result::Result<T, String> {
        let request = self.http.get(format!("{}{path}", self.base));
        self.send(request).await
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> std::result::Result<T, String> {
        let mut request = self.http.post(format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        self.send(request).await
    }

    async fn post_empty(
        &self,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> std::result::Result<(), String> {
        let mut request = self.http.post(format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(describe_failure(
            status,
            response.text().await.unwrap_or_default(),
        ))
    }

    async fn delete<T: DeserializeOwned>(&self, path: &str) -> std::result::Result<T, String> {
        let request = self.http.delete(format!("{}{path}", self.base));
        self.send(request).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> std::result::Result<T, String> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| transport_error(&error))?;
        if !status.is_success() {
            return Err(describe_failure(status, body));
        }
        serde_json::from_str(&body).map_err(|error| format!("unexpected response: {error}"))
    }
}

fn transport_error(error: &reqwest::Error) -> String {
    if error.is_connect() {
        "the Refinery service is not answering".to_owned()
    } else if error.is_timeout() {
        "the Refinery service took too long to answer".to_owned()
    } else {
        format!("could not reach the Refinery service: {error}")
    }
}

/// Turn an error response into one sentence a person can act on.
pub fn describe_failure(status: reqwest::StatusCode, body: String) -> String {
    match serde_json::from_str::<ApiError>(&body) {
        Ok(error) if error.issues.is_empty() => error.message,
        Ok(error) => {
            let issues: Vec<String> = error
                .issues
                .iter()
                .map(|issue| {
                    if issue.field.is_empty() {
                        issue.message.clone()
                    } else {
                        format!("{}: {}", issue.field, issue.message)
                    }
                })
                .collect();
            format!("{} ({})", error.message, issues.join("; "))
        }
        Err(_) => {
            let text = body.trim();
            if text.is_empty() {
                format!("the service answered {status}")
            } else {
                format!("the service answered {status}: {text}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_errors_are_flattened_to_one_sentence() {
        let body = serde_json::json!({
            "schema_version": 1,
            "code": "invalid",
            "message": "the request is invalid",
            "retry_class": "non_retryable",
            "issues": [
                { "field": "transcript", "code": "empty", "message": "must not be empty" },
                { "field": "", "code": "malformed", "message": "unreadable" }
            ]
        })
        .to_string();
        assert_eq!(
            describe_failure(reqwest::StatusCode::BAD_REQUEST, body),
            "the request is invalid (transcript: must not be empty; unreadable)"
        );
        assert_eq!(
            describe_failure(reqwest::StatusCode::BAD_GATEWAY, "  ".into()),
            "the service answered 502 Bad Gateway"
        );
    }
}
