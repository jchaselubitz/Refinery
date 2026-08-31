//! The local browser interface, exercised against a live daemon.
//!
//! M8's exit criteria name four interactions that must work through the
//! interface: answering a question, retrying a delivery, cancelling a case,
//! and copying the prompt. This file runs a real daemon — the same router, the
//! same job loop, the same shutdown ordering that `refinery serve` composes —
//! and drives it with exactly the requests the page's own code issues.
//!
//! Two things are doubles and nothing else is: the model provider replays
//! recorded `generateContent` bodies, and the destination is a local server
//! that can be told to refuse. Storage, the state machine, the job runner, the
//! question service, delivery bookkeeping, and every HTTP route are real.
//!
//! The answer payloads below are transcribed from the page's `collectAnswers`
//! function rather than built from the Rust types, deliberately. A test that
//! serializes `AnswerSet` would prove the server agrees with itself; these
//! prove the server accepts what the browser actually sends.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Router};
use serde_json::{json, Value};

use refinery::cases::Worker;
use refinery::config::{Config, DataDir};
use refinery::domain::{
    Destination, MessageRole, OverlordDestination, RefinementRequest, SchemaVersion, SourceRef,
    SourceSystem, Transcript, TranscriptMessage,
};
use refinery::storage::Storage;

const SUBMIT_VALID: &str = include_str!("../fixtures/gemini/generate_submit_valid.json");

/// An `ask_user` turn covering all three response types.
///
/// Constructed rather than recorded: the recorded fixtures under
/// `tests/fixtures/gemini` are sanitized captures of real provider traffic,
/// and this one exists to make the interface render every form control it
/// supports in a single case. It is a valid `generateContent` body either way.
const ASK_USER_EVERY_TYPE: &str = r#"{
  "candidates": [
    {
      "content": {
        "role": "model",
        "parts": [
          {
            "functionCall": {
              "name": "ask_user",
              "args": {
                "prompt": "Three things are ambiguous before I can write the prompt.",
                "questions": [
                  {
                    "label": "What should the retry budget be?",
                    "description": "A sentence is enough.",
                    "response_type": "free_text",
                    "required": true
                  },
                  {
                    "label": "Which endpoint should the retries cover?",
                    "response_type": "single_choice",
                    "choices": [
                      { "value": "submit", "label": "Submit only" },
                      { "value": "all", "label": "Every outbound call" }
                    ],
                    "required": true
                  },
                  {
                    "label": "Which failures count as transient?",
                    "response_type": "multiple_choice",
                    "choices": [
                      { "value": "timeout" },
                      { "value": "5xx" },
                      { "value": "connection_reset" }
                    ],
                    "required": false
                  }
                ]
              }
            }
          }
        ]
      },
      "finishReason": "STOP"
    }
  ],
  "usageMetadata": { "promptTokenCount": 100, "candidatesTokenCount": 40, "totalTokenCount": 140 }
}"#;

/* Doubles ----------------------------------------------------------------- */

/// A `generateContent` endpoint that replays a scripted conversation.
struct FakeProvider {
    turns: Mutex<VecDeque<&'static str>>,
}

async fn generate(State(provider): State<Arc<FakeProvider>>, _body: String) -> impl IntoResponse {
    match provider.turns.lock().unwrap().pop_front() {
        Some(body) => (StatusCode::OK, body.to_owned()),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":{"message":"no further turns were scripted"}}"#.to_owned(),
        ),
    }
}

/// A destination that refuses until it is told to accept.
///
/// It refuses with `400`, which classifies as non-retryable, so the first
/// delivery exhausts on its first attempt and the case lands in `failed` with
/// its prompt preserved. That is the state the interface offers a person the
/// retry button from, and reaching it in one attempt keeps the test off the
/// job runner's backoff schedule.
#[derive(Default)]
struct FakeDestination {
    accepting: AtomicBool,
    attempts: AtomicUsize,
    keys: Mutex<Vec<String>>,
}

async fn deliver(
    State(destination): State<Arc<FakeDestination>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> impl IntoResponse {
    destination.attempts.fetch_add(1, Ordering::SeqCst);
    if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        destination.keys.lock().unwrap().push(key.to_owned());
    }
    let envelope: Value = serde_json::from_str(&body).expect("the envelope is JSON");
    assert!(
        envelope["prompt"]["acceptance_criteria"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "a delivered prompt must carry acceptance criteria"
    );
    if destination.accepting.load(Ordering::SeqCst) {
        (
            StatusCode::OK,
            json!({
                "schema_version": SchemaVersion::CURRENT,
                "accepted": true,
                "destination_reference": "coo:885.test",
                "received_at": chrono::Utc::now(),
            })
            .to_string(),
        )
    } else {
        (
            StatusCode::BAD_REQUEST,
            r#"{"message":"the mission is not accepting submissions"}"#.to_owned(),
        )
    }
}

/// A destination that applies a delivery, loses the first response, then
/// deduplicates the safe retry by the stable delivery id.
#[derive(Default)]
struct LostResponseDestination {
    attempts: AtomicUsize,
    applied: Mutex<HashSet<String>>,
    keys: Mutex<Vec<String>>,
}

async fn deliver_after_lost_response(
    State(destination): State<Arc<LostResponseDestination>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> impl IntoResponse {
    let attempt = destination.attempts.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) {
        destination.keys.lock().unwrap().push(key.to_owned());
    }
    let envelope: Value = serde_json::from_str(&body).expect("the envelope is JSON");
    let delivery_id = envelope["delivery_id"]
        .as_str()
        .expect("a stable delivery id")
        .to_owned();
    destination.applied.lock().unwrap().insert(delivery_id);

    if attempt == 1 {
        // The destination applied the envelope, but Refinery observed only a
        // transient failure. Retrying is safe because delivery_id is stable.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"message":"response lost after commit"}"#.to_owned(),
        );
    }
    (
        StatusCode::OK,
        json!({
            "schema_version": SchemaVersion::CURRENT,
            "accepted": true,
            "destination_reference": "coo:885.retry-test",
            "received_at": chrono::Utc::now(),
        })
        .to_string(),
    )
}

async fn serve_double(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), handle)
}

/* The daemon under test --------------------------------------------------- */

struct Daemon {
    base: String,
    token: String,
    client: reqwest::Client,
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    _temp: tempfile::TempDir,
}

/// Pin every credential read and write in this binary to the fallback file.
///
/// Without it these tests would store a provider key and a bearer token in the
/// keychain of whoever is running them, which is exactly what the credential
/// module's environment override exists to prevent.
fn pin_credentials_to_a_file() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var(
            refinery::config::credentials::CREDENTIAL_BACKEND_ENV,
            "file",
        );
    });
}

impl Daemon {
    async fn start(provider_base: &str) -> Self {
        pin_credentials_to_a_file();
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::at(temp.path().join("data")).expect("resolve");
        let config = Config::load_from(data_dir).expect("load configuration");

        // The credential the worker reads before it will start a backend. It
        // is stored through the real credential layer, so the daemon under
        // test resolves it exactly as an installed one would.
        refinery::config::credentials::CredentialStore::open(&config.data_dir.credentials_dir())
            .set(
                &refinery::config::credentials::CredentialKey::provider_api_key("gemini"),
                &refinery::domain::SecretString::new("test-key"),
            )
            .expect("store the provider key");

        let token = refinery::api::ensure_token(&config).expect("token");
        let store = Storage::open(config.data_dir.database_file())
            .await
            .expect("open storage");
        let worker = Worker::new(Arc::new(config.clone()), store.clone())
            .with_provider_base(format!("{provider_base}/v1beta"));

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("address");
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            refinery::api::serve_on(listener, &config, store, worker, receiver)
                .await
                .expect("the daemon serves");
        });

        Self {
            base: format!("http://{address}"),
            token,
            client: reqwest::Client::new(),
            shutdown,
            task,
            _temp: temp,
        }
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("request");
        assert!(response.status().is_success(), "GET {path} failed");
        response.json().await.expect("JSON body")
    }

    async fn post(&self, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut request = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.expect("request")
    }

    /// Poll the case detail until a condition holds, the way the page does.
    async fn until(&self, case_id: &str, label: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut last = Value::Null;
        while tokio::time::Instant::now() < deadline {
            last = self.get(&format!("/v1/refinements/{case_id}")).await;
            if ready(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        panic!(
            "timed out waiting for {label}; last state was {}",
            last["state"]
        );
    }

    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

fn request_for(destination_base: &str, request_id: &str) -> RefinementRequest {
    RefinementRequest {
        schema_version: SchemaVersion::CURRENT,
        request_id: request_id.into(),
        idempotency_key: format!("key-{request_id}"),
        source: SourceRef {
            system: SourceSystem::Overlord,
            instance: "local".into(),
            callback: None,
        },
        transcript: Transcript::new(vec![TranscriptMessage {
            id: None,
            role: MessageRole::User,
            content: "The client gives up on the first transient failure.".into(),
            author: None,
            created_at: None,
        }]),
        task_hint: Some("Add retries".into()),
        attachments: Vec::new(),
        repository: None,
        destination: Destination::Overlord(OverlordDestination {
            base_url: destination_base.to_owned(),
            mission_id: Some("coo:885".into()),
            objective_id: None,
            bearer_token: None,
        }),
        metadata: Default::default(),
    }
}

/* The exit criteria ------------------------------------------------------- */

/// The whole interface path: a case arrives, the agent asks, a person answers
/// through the form, delivery fails, the person retries it, and the prompt is
/// there to copy.
#[tokio::test]
async fn every_interface_interaction_works_against_a_live_daemon() {
    let provider = Arc::new(FakeProvider {
        turns: Mutex::new(VecDeque::from([ASK_USER_EVERY_TYPE, SUBMIT_VALID])),
    });
    let (provider_base, _provider_task) = serve_double(
        Router::new()
            .route("/v1beta/models/{model}", post(generate))
            .with_state(provider),
    )
    .await;

    let destination = Arc::new(FakeDestination::default());
    let (destination_base, _destination_task) = serve_double(
        Router::new()
            .route("/v1/refinery/deliveries", post(deliver))
            .with_state(destination.clone()),
    )
    .await;

    let daemon = Daemon::start(&provider_base).await;

    // --- Ingress: the case appears in the list view ------------------------
    let accepted: Value = daemon
        .post(
            "/v1/refinements",
            Some(serde_json::to_value(request_for(&destination_base, "r-ui")).unwrap()),
        )
        .await
        .json()
        .await
        .expect("an acceptance body");
    assert_eq!(accepted["created"], true);
    let case_id = accepted["case_id"].as_str().expect("a case id").to_owned();

    let listed = daemon.get("/v1/refinements").await;
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == case_id.as_str()),
        "the case list must show the new case"
    );

    // --- Answering a question ---------------------------------------------
    let detail = daemon
        .until(&case_id, "the agent to ask a question", |detail| {
            detail["pending_question"].is_object()
        })
        .await;
    let pending = &detail["pending_question"];
    let questions = pending["questions"].as_array().expect("questions");
    assert_eq!(questions.len(), 3, "the form must render all three types");

    let answered_at = chrono::Utc::now().to_rfc3339();
    let answers = json!({
        "schema_version": pending["schema_version"],
        "case_id": pending["case_id"],
        "question_request_id": pending["id"],
        "answers": [
            {
                "question_id": questions[0]["id"],
                "value": { "type": "text", "text": "Three attempts." },
                "answered_at": answered_at,
                "answered_by": "local-ui",
            },
            {
                "question_id": questions[1]["id"],
                "value": { "type": "choice", "value": "all" },
                "answered_at": answered_at,
                "answered_by": "local-ui",
            },
            {
                "question_id": questions[2]["id"],
                "value": { "type": "choices", "values": ["timeout", "5xx"] },
                "answered_at": answered_at,
                "answered_by": "local-ui",
            },
        ],
    });
    let response = daemon
        .post(&format!("/v1/refinements/{case_id}/answers"), Some(answers))
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // --- Delivery fails, and the detail view says why ----------------------
    let failed = daemon
        .until(&case_id, "the first delivery to fail", |detail| {
            detail["state"] == "failed"
        })
        .await;
    assert_eq!(destination.attempts.load(Ordering::SeqCst), 1);
    let deliveries = failed["deliveries"].as_array().expect("deliveries");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0]["status"], "failed");
    assert_eq!(deliveries[0]["retry_class"], "non_retryable");
    assert!(
        deliveries[0]["error"]
            .as_str()
            .is_some_and(|e| !e.is_empty()),
        "a failed delivery must say what went wrong"
    );

    // --- Copying the prompt ------------------------------------------------
    // The output survives the failure, which is what makes the retry
    // meaningful and what the copy action reads.
    let prompt = &failed["output"]["prompt"];
    assert_eq!(
        prompt["title"],
        "Add bounded retries to the outbound client"
    );
    assert!(!prompt["acceptance_criteria"].as_array().unwrap().is_empty());
    assert!(prompt["prompt"].as_str().unwrap().contains("retry"));

    // The answered question and its three responses are on the detail view.
    let threads = failed["questions"].as_array().expect("question threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["request"]["status"], "answered");
    assert_eq!(threads[0]["answers"].as_array().unwrap().len(), 3);

    // --- Retrying the delivery ---------------------------------------------
    destination.accepting.store(true, Ordering::SeqCst);
    let retry = daemon
        .post(&format!("/v1/refinements/{case_id}/deliveries"), None)
        .await;
    assert_eq!(retry.status(), StatusCode::ACCEPTED);

    let completed = daemon
        .until(&case_id, "the retried delivery to be accepted", |detail| {
            detail["state"] == "completed"
        })
        .await;
    assert_eq!(destination.attempts.load(Ordering::SeqCst), 2);
    let deliveries = completed["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 2, "each user retry is its own delivery");
    assert_eq!(deliveries[1]["status"], "accepted");

    // Every attempt presented a distinct idempotency key, so a destination can
    // tell a retry of one submission from a second submission.
    let keys = destination.keys.lock().unwrap().clone();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);

    // --- Cancelling a case -------------------------------------------------
    // A completed case cannot be cancelled, and the daemon says so rather than
    // relying on the page having disabled the button.
    let refused = daemon
        .post(&format!("/v1/refinements/{case_id}/cancel"), None)
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

    let second: Value = daemon
        .post(
            "/v1/refinements",
            Some(serde_json::to_value(request_for(&destination_base, "r-cancel")).unwrap()),
        )
        .await
        .json()
        .await
        .expect("an acceptance body");
    let second_id = second["case_id"].as_str().unwrap().to_owned();
    let cancelled = daemon
        .post(&format!("/v1/refinements/{second_id}/cancel"), None)
        .await;
    assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);
    let detail = daemon.get(&format!("/v1/refinements/{second_id}")).await;
    assert_eq!(detail["state"], "cancelled");

    // --- The supporting views ----------------------------------------------
    let health = daemon.get("/v1/health").await;
    assert_eq!(health["provider_credential"], true);
    assert!(health["cases_by_state"]["completed"].as_u64().unwrap() >= 1);

    let logs = daemon.get("/v1/logs?lines=5").await;
    assert!(logs["lines"].is_array());

    daemon.stop().await;
}

/// Stage 1's delivery guarantee: a transient lost response produces another
/// HTTP attempt, while the stable delivery id lets the destination apply the
/// prompt exactly once. The event history must tell the truth about attempt 2.
#[tokio::test]
async fn a_safe_delivery_retry_is_applied_exactly_once() {
    let provider = Arc::new(FakeProvider {
        turns: Mutex::new(VecDeque::from([SUBMIT_VALID])),
    });
    let (provider_base, _provider_task) = serve_double(
        Router::new()
            .route("/v1beta/models/{model}", post(generate))
            .with_state(provider),
    )
    .await;

    let destination = Arc::new(LostResponseDestination::default());
    let (destination_base, _destination_task) = serve_double(
        Router::new()
            .route("/v1/refinery/deliveries", post(deliver_after_lost_response))
            .with_state(destination.clone()),
    )
    .await;
    let daemon = Daemon::start(&provider_base).await;

    let accepted: Value = daemon
        .post(
            "/v1/refinements",
            Some(serde_json::to_value(request_for(&destination_base, "r-safe-retry")).unwrap()),
        )
        .await
        .json()
        .await
        .expect("an acceptance body");
    let case_id = accepted["case_id"].as_str().unwrap().to_owned();
    let completed = daemon
        .until(&case_id, "the retry to complete", |detail| {
            detail["state"] == "completed"
        })
        .await;

    assert_eq!(destination.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        destination.applied.lock().unwrap().len(),
        1,
        "the destination applies one stable delivery"
    );
    let keys = destination.keys.lock().unwrap().clone();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1], "each transport attempt is identifiable");
    assert_eq!(completed["deliveries"][0]["attempt"], 2);

    let delivery_events: Vec<&Value> = completed["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "delivery_attempted")
        .collect();
    assert_eq!(delivery_events.len(), 2);
    assert_eq!(delivery_events[0]["attempt"], 1);
    assert_eq!(delivery_events[0]["outcome"], "failed");
    assert_eq!(delivery_events[1]["attempt"], 2);
    assert_eq!(delivery_events[1]["outcome"], "succeeded");

    daemon.stop().await;
}

/// The shell is what a browser loads first, so it has to arrive intact from
/// the binary with no build step and no file beside it.
#[tokio::test]
async fn the_interface_is_served_from_the_binary() {
    let (provider_base, _task) = serve_double(Router::new()).await;
    let daemon = Daemon::start(&provider_base).await;

    let shell = reqwest::get(format!("{}/", daemon.base))
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert!(shell.contains("<title>Refinery</title>"));
    assert!(shell.contains("/assets/app.js"));

    let script = reqwest::get(format!("{}/assets/app.js", daemon.base))
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert!(
        script.contains("EventSource"),
        "the page follows the stream"
    );

    daemon.stop().await;
}
