//! The authenticated loopback HTTP API and the local browser interface.
//!
//! Axum bound to loopback only, with a uniform error body derived from
//! [`crate::error::AppError`]. It carries three surfaces that happen to share a
//! port: Overlord's ingress and question routes, the case and settings routes
//! the local interface reads, and the embedded interface itself.
//!
//! ## Authentication
//!
//! Every `/v1` route requires the locally generated bearer token, presented
//! either in an `Authorization` header or as a `token` query parameter. The
//! query parameter is not a convenience: `EventSource` cannot set headers, so
//! a browser has no other way to authenticate a server-sent event stream, and
//! `refinery open` uses the same form to hand the token to a fresh browser.
//! The page strips it from the address bar on load.
//!
//! The interface's own assets are served without a token. They are three files
//! compiled into a binary the user already possesses and they contain no case
//! data; refusing them would only prevent the page that asks for a token from
//! rendering. Everything with data behind it stays authenticated.
//!
//! ## Bounds
//!
//! Every list route has a ceiling a caller cannot raise. The interface asks
//! for a case's whole history and gets the most recent slice of it, because a
//! long-running case's history is not something a browser should be handed in
//! one response.

use std::{str::FromStr, sync::Arc, time::Duration};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::{
        credentials::{CredentialKey, CredentialStore},
        Config, DataDir,
    },
    domain::{AnswerSet, ApiError, CaseId, CaseState, RefinementRequest, SchemaVersion},
    error::{AppError, Result},
    storage::{CreateCaseResult, Storage, MAX_LIST_CASES, MAX_STREAM_EVENTS},
};

pub mod assets;

/// How often a following event stream looks for new events.
const STREAM_POLL: Duration = Duration::from_millis(500);
/// How often an idle stream emits a keep-alive comment.
const STREAM_KEEPALIVE: Duration = Duration::from_secs(15);
/// The largest log tail one request may ask for.
const MAX_LOG_LINES: usize = 2_000;
/// How much of a log file's tail is read to satisfy a request.
const LOG_TAIL_BYTES: u64 = 512 * 1024;

/// Shared server state. The token is only ever compared, never returned.
///
/// The credential store is opened once and held rather than opened per
/// request. Opening it probes the operating-system keychain, and a health
/// endpoint the interface polls should not ask the platform that question
/// several times a minute.
#[derive(Clone)]
pub struct ApiState {
    store: Storage,
    token: Arc<str>,
    settings: crate::config::Settings,
    data_dir: DataDir,
    credentials: CredentialStore,
}

/// Build the local-only API router.
///
/// Separated from binding so route behaviour can be tested hermetically, and
/// so the service supervisor and the tests exercise exactly the same routes.
pub fn router(
    store: Storage,
    token: impl Into<String>,
    settings: crate::config::Settings,
    data_dir: DataDir,
    credentials: CredentialStore,
) -> Router {
    let state = ApiState {
        store,
        token: Arc::from(token.into()),
        settings,
        data_dir,
        credentials,
    };
    let api = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/refinements", post(create_case).get(list_cases))
        .route("/v1/refinements/{id}", get(case))
        .route("/v1/refinements/{id}/answers", post(answer))
        .route("/v1/refinements/{id}/cancel", post(cancel))
        .route("/v1/refinements/{id}/deliveries", post(retry_delivery))
        .route("/v1/refinements/{id}/events", get(events))
        .route("/v1/repositories", get(repositories).post(add_repository))
        .route("/v1/settings", get(get_settings))
        .route("/v1/logs", get(logs))
        .layer(middleware::from_fn_with_state(state.clone(), authorize));
    let interface = Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/assets/{*path}", get(asset));
    Router::new().merge(api).merge(interface).with_state(state)
}

/// Bind the API to the configured loopback address and start the job loop.
///
/// The two are started together deliberately. A daemon that served the
/// interface without running jobs would accept an answer, show it recorded,
/// and never resume the case — the failure would look like the model hanging
/// rather than like a process that was never asked to do the work.
pub async fn serve(config: &Config, store: Storage) -> Result<()> {
    let address = config.settings.api.bind_address();
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|source| AppError::Io {
            path: std::path::PathBuf::from(address.to_string()),
            source,
        })?;
    let (signal, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = signal.send(true);
    });
    let worker = crate::cases::Worker::new(Arc::new(config.clone()), store.clone());
    tracing::info!(%address, "serving the local interface and API");
    serve_on(listener, config, store, worker, shutdown).await
}

/// Serve on an already-bound listener until `shutdown` is set.
///
/// Split out from [`serve`] so a test can run the whole daemon — the same
/// router, the same job loop, the same shutdown ordering — on an ephemeral
/// port with a worker pointed at a local provider double. A test that composed
/// those pieces itself would be asserting against an arrangement no user runs.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    config: &Config,
    store: Storage,
    worker: crate::cases::Worker,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let credentials = CredentialStore::open(&config.data_dir.credentials_dir());
    let token = ensure_token(config)?;
    let worker_task = tokio::spawn(worker.run_until(shutdown.clone()));
    let router = router(
        store,
        token,
        config.settings.clone(),
        config.data_dir.clone(),
        credentials,
    );
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            // A closed sender means the owner went away, which is a shutdown.
            while shutdown.changed().await.is_ok() {
                if *shutdown.borrow() {
                    return;
                }
            }
        })
        .await
        .map_err(|error| AppError::Internal(anyhow::Error::new(error)));
    let _ = worker_task.await;
    result
}

/// Read the stored bearer token, generating and storing one on first use.
pub fn ensure_token(config: &Config) -> Result<String> {
    let credentials = CredentialStore::open(&config.data_dir.credentials_dir());
    match credentials.get(&CredentialKey::ApiBearerToken)? {
        Some(token) => Ok(token.expose().to_owned()),
        None => {
            let token = uuid::Uuid::new_v4().to_string();
            credentials.set(
                &CredentialKey::ApiBearerToken,
                &crate::domain::SecretString::new(&token),
            )?;
            Ok(token)
        }
    }
}

async fn authorize(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned)
        .or_else(|| query_token(uri.query()));
    let valid = presented.is_some_and(|token| constant_time_eq(&token, state.token.as_ref()));
    if valid {
        next.run(request).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            AppError::PolicyDenied {
                message: "a valid local bearer token is required".into(),
            },
        )
    }
}

fn query_token(query: Option<&str>) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
}

/// Compare two tokens without an early return on the first differing byte.
///
/// The listener is loopback-only, so this is defence in depth rather than a
/// response to a realistic remote attacker. It costs four lines.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let mut difference = (left.len() ^ right.len()) as u8;
    for index in 0..left.len().max(right.len()) {
        difference |=
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0);
    }
    difference == 0
}

/* The embedded interface -------------------------------------------------- */

async fn index() -> Response {
    serve_asset(&assets::INDEX)
}

async fn asset(Path(path): Path<String>) -> Response {
    match assets::lookup(&format!("/assets/{path}")) {
        Some(asset) => serve_asset(asset),
        None => app_error(AppError::NotFound {
            kind: "asset",
            id: path,
        }),
    }
}

fn serve_asset(asset: &'static assets::Asset) -> Response {
    (
        [
            (header::CONTENT_TYPE, asset.content_type),
            // The assets change with the binary, and a stale shell against a
            // newer daemon is a confusing failure to debug.
            (header::CACHE_CONTROL, "no-cache"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        asset.body,
    )
        .into_response()
}

/* Health and settings ----------------------------------------------------- */

#[derive(Serialize)]
struct Health {
    schema_version: SchemaVersion,
    status: &'static str,
    version: &'static str,
    data_dir: String,
    listen: String,
    provider: String,
    model: String,
    provider_credential: bool,
    pending_migrations: usize,
    integrity_problems: Vec<String>,
    cases_by_state: std::collections::BTreeMap<String, u64>,
    jobs_by_status: std::collections::BTreeMap<String, u64>,
    case_states: Vec<&'static str>,
}

async fn health(State(state): State<ApiState>) -> Response {
    let provider = state.settings.provider.backend.clone();
    let provider_credential = state
        .credentials
        .get(&CredentialKey::provider_api_key(&provider))
        .ok()
        .flatten()
        .is_some();
    let pending_migrations = state
        .store
        .pending_migrations()
        .await
        .map(|items| items.len())
        .unwrap_or_default();
    let integrity_problems = state.store.integrity_problems().await.unwrap_or_default();
    let cases_by_state = match state.store.case_counts_by_state().await {
        Ok(counts) => counts,
        Err(error) => return app_error(error),
    };
    let jobs_by_status = state.store.job_counts_by_status().await.unwrap_or_default();
    Json(Health {
        schema_version: SchemaVersion::CURRENT,
        status: if integrity_problems.is_empty() && pending_migrations == 0 {
            "ok"
        } else {
            "degraded"
        },
        version: env!("CARGO_PKG_VERSION"),
        data_dir: state.data_dir.root().display().to_string(),
        listen: state.settings.api.bind_address().to_string(),
        model: state.settings.provider.model.clone(),
        provider,
        provider_credential,
        pending_migrations,
        integrity_problems,
        cases_by_state,
        jobs_by_status,
        case_states: CaseState::ALL.iter().map(|state| state.as_str()).collect(),
    })
    .into_response()
}

async fn get_settings(State(state): State<ApiState>) -> Json<crate::config::Settings> {
    Json(state.settings)
}

/* Cases ------------------------------------------------------------------- */

#[derive(Serialize)]
struct Accepted {
    schema_version: SchemaVersion,
    case_id: CaseId,
    created: bool,
}

async fn create_case(
    State(state): State<ApiState>,
    Json(request): Json<RefinementRequest>,
) -> Response {
    match state.store.create_case_idempotent(&request).await {
        Ok(result) => Json(Accepted {
            schema_version: SchemaVersion::CURRENT,
            case_id: result.case_id(),
            created: matches!(result, CreateCaseResult::Created(_)),
        })
        .into_response(),
        Err(e) => app_error(e),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
    limit: Option<u32>,
}

async fn list_cases(State(state): State<ApiState>, Query(query): Query<ListQuery>) -> Response {
    let parsed = match query.state.as_deref().filter(|value| !value.is_empty()) {
        Some(value) => match parse_state(value) {
            Some(state) => Some(state),
            None => return app_error(AppError::invalid("unknown case state")),
        },
        None => None,
    };
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_LIST_CASES);
    match state.store.list_case_summaries(parsed, limit).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => app_error(e),
    }
}

async fn case(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    match parse_id(&id) {
        Ok(id) => match state.store.case_detail(id).await {
            Ok(value) => Json(value).into_response(),
            Err(e) => app_error(e),
        },
        Err(e) => app_error(e),
    }
}

async fn answer(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<AnswerSet>,
) -> Response {
    match parse_id(&id) {
        Ok(id) => {
            if body.case_id != id {
                return app_error(AppError::invalid("answer case_id does not match the route"));
            }
            match state.store.apply_answer(&body).await {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => app_error(e),
            }
        }
        Err(e) => app_error(e),
    }
}

async fn cancel(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    match parse_id(&id) {
        Ok(id) => match state.store.cancel_case(id, None).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => app_error(e),
        },
        Err(e) => app_error(e),
    }
}

/// Queue another delivery for a case that failed with its prompt preserved.
///
/// The route is a `POST` to the case's delivery collection rather than a
/// `retry` verb because that is what it is: a request for one more delivery of
/// an output that already exists. Storage refuses it from any state but
/// `failed`, so an impatient click on a running case is a `400` and not a
/// second delivery racing the first.
async fn retry_delivery(State(state): State<ApiState>, Path(id): Path<String>) -> Response {
    match parse_id(&id) {
        Ok(id) => match state.store.request_delivery_retry(id).await {
            Ok(()) => StatusCode::ACCEPTED.into_response(),
            Err(e) => app_error(e),
        },
        Err(e) => app_error(e),
    }
}

/* Repositories ------------------------------------------------------------ */

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterRepository {
    path: String,
    max_file_bytes: Option<u64>,
    max_results: Option<usize>,
    respect_ignore_files: Option<bool>,
}

async fn repositories(State(state): State<ApiState>) -> Response {
    match state.store.list_repositories().await {
        Ok(items) => {
            Json(items.into_iter().map(repository_view).collect::<Vec<_>>()).into_response()
        }
        Err(e) => app_error(e),
    }
}

/// Register a repository root from the interface.
///
/// The policy defaults come from settings, exactly as `refinery repository
/// add` computes them, and the request may only lower them. A browser form
/// must not be able to widen what the agent can read past what the
/// installation's own limits allow.
async fn add_repository(
    State(state): State<ApiState>,
    Json(body): Json<RegisterRepository>,
) -> Response {
    let root = match crate::repositories::canonical_root(std::path::Path::new(&body.path)) {
        Ok(root) => root,
        Err(e) => return app_error(e),
    };
    let mut policy = crate::repositories::RepositoryPolicy::from_limits(&state.settings.limits);
    if let Some(value) = body.max_file_bytes {
        policy.max_file_bytes = policy.max_file_bytes.min(value);
    }
    if let Some(value) = body.max_results {
        policy.max_results = policy.max_results.min(value);
    }
    if let Some(value) = body.respect_ignore_files {
        policy.respect_ignore_files = value;
    }
    match state
        .store
        .register_repository(&root, &policy, chrono::Utc::now())
        .await
    {
        Ok((repository, _)) => Json(repository_view(repository)).into_response(),
        Err(e) => app_error(e),
    }
}

fn repository_view(repository: crate::repositories::Repository) -> serde_json::Value {
    serde_json::json!({
        "id": repository.id,
        "root": repository.root,
        "label": repository.label(),
        "is_git_work_tree": repository.is_git_work_tree(),
        "policy": repository.policy,
        "registered_at": repository.registered_at,
    })
}

/* Events ------------------------------------------------------------------ */

#[derive(Deserialize)]
struct EventQuery {
    /// Keep the connection open and send events as they are appended.
    ///
    /// Read as a string rather than a `bool` because this flag is written by
    /// hand in a URL as often as it is generated: `follow=1`, `follow`, and
    /// `follow=true` all mean the same thing to a person, and rejecting two of
    /// the three with a `400` would be a puzzle rather than a validation.
    follow: Option<String>,
    /// Resume after a sequence position the client has already rendered.
    after: Option<u64>,
}

impl EventQuery {
    fn follows(&self) -> bool {
        match self.follow.as_deref() {
            None => false,
            Some(value) => !matches!(value, "0" | "false" | "no"),
        }
    }
}

/// Serve a case's history as JSON or as server-sent events.
///
/// Content negotiation rather than two routes: the interface and a scripted
/// client want the same history, and only differ in whether they want to be
/// told about the next one. `follow` then decides whether the stream ends
/// after the backlog or stays open, which keeps a one-shot `curl` from hanging
/// while the browser still gets live updates.
async fn events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Response {
    let case_id = match parse_id(&id) {
        Ok(id) => id,
        Err(e) => return app_error(e),
    };
    let after = query.after.or_else(|| last_event_id(&headers)).unwrap_or(0);
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
        || query.follows();

    if !wants_stream {
        return match state
            .store
            .case_events_after(case_id, after, MAX_STREAM_EVENTS)
            .await
        {
            Ok(items) => Json(items).into_response(),
            Err(e) => app_error(e),
        };
    }

    // Confirm the case exists before opening a stream, so a bad identifier is
    // a 404 rather than a connection that silently never sends anything.
    if let Err(error) = state.store.case_state(case_id).await {
        return app_error(error);
    }

    let follow = query.follows();
    let store = state.store.clone();
    let stream = async_stream::stream! {
        let mut cursor = after;
        loop {
            match store.case_events_after(case_id, cursor, MAX_STREAM_EVENTS).await {
                Ok(batch) => {
                    for event in batch {
                        cursor = event.sequence;
                        yield sse_event(&event);
                    }
                }
                Err(error) => {
                    yield Ok(Event::default().event("error").data(
                        serde_json::to_string(&ApiError::from_app_error(&error))
                            .unwrap_or_else(|_| "{}".into()),
                    ));
                    return;
                }
            }
            if !follow {
                return;
            }
            tokio::time::sleep(STREAM_POLL).await;
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(STREAM_KEEPALIVE))
        .into_response()
}

fn sse_event(
    event: &crate::domain::CaseEvent,
) -> std::result::Result<Event, std::convert::Infallible> {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    Ok(Event::default()
        .id(event.sequence.to_string())
        .event("case_event")
        .data(data))
}

fn last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/* Logs -------------------------------------------------------------------- */

#[derive(Deserialize)]
struct LogQuery {
    lines: Option<usize>,
}

#[derive(Serialize)]
struct LogTail {
    schema_version: SchemaVersion,
    file: Option<String>,
    lines: Vec<String>,
}

/// Return the tail of the newest log file.
///
/// Only the tail, and only of the newest file: this is the diagnostic view a
/// person opens when something has just gone wrong, not an archive browser.
/// Records are written through the redacting writer, so what is on disk is
/// already free of registered secrets and this route does not have to filter.
async fn logs(State(state): State<ApiState>, Query(query): Query<LogQuery>) -> Response {
    let wanted = query.lines.unwrap_or(200).clamp(1, MAX_LOG_LINES);
    match read_log_tail(&state.data_dir.log_dir(), wanted).await {
        Ok((file, lines)) => Json(LogTail {
            schema_version: SchemaVersion::CURRENT,
            file,
            lines,
        })
        .into_response(),
        Err(e) => app_error(e),
    }
}

async fn read_log_tail(
    log_dir: &std::path::Path,
    wanted: usize,
) -> Result<(Option<String>, Vec<String>)> {
    let mut entries = match tokio::fs::read_dir(log_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((None, Vec::new()))
        }
        Err(source) => {
            return Err(AppError::Io {
                path: log_dir.to_path_buf(),
                source,
            })
        }
    };
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("log") {
            continue;
        }
        let modified = entry
            .metadata()
            .await
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, path));
        }
    }
    let Some((_, path)) = newest else {
        return Ok((None, Vec::new()));
    };

    let bytes = tail_bytes(&path).await?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    // A tail read may begin mid-line; drop that fragment rather than show it.
    if bytes.len() as u64 >= LOG_TAIL_BYTES && !lines.is_empty() {
        lines.remove(0);
    }
    let start = lines.len().saturating_sub(wanted);
    Ok((
        Some(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ),
        lines.split_off(start),
    ))
}

async fn tail_bytes(path: &std::path::Path) -> Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| AppError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let length = file
        .metadata()
        .await
        .map_err(|source| AppError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if length > LOG_TAIL_BYTES {
        file.seek(std::io::SeekFrom::Start(length - LOG_TAIL_BYTES))
            .await
            .map_err(|source| AppError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .await
        .map_err(|source| AppError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(buffer)
}

/* Shared ------------------------------------------------------------------ */

fn parse_id(value: &str) -> Result<CaseId> {
    CaseId::from_str(value).map_err(|_| AppError::invalid("invalid case id"))
}

fn parse_state(value: &str) -> Option<CaseState> {
    CaseState::ALL
        .iter()
        .copied()
        .find(|state| state.as_str() == value)
}

fn app_error(error: AppError) -> Response {
    error_response(
        match error.code() {
            "not_found" => StatusCode::NOT_FOUND,
            "idempotency_conflict" => StatusCode::CONFLICT,
            "policy_denied" => StatusCode::FORBIDDEN,
            "invalid_request" => StatusCode::BAD_REQUEST,
            "needs_user" => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        error,
    )
}

fn error_response(status: StatusCode, error: AppError) -> Response {
    (status, Json(ApiError::from_app_error(&error))).into_response()
}

#[cfg(test)]
mod tests;
