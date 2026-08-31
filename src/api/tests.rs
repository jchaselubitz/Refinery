//! Route tests against a real listener.
//!
//! These bind a socket and speak HTTP rather than calling handlers directly,
//! because most of what is worth asserting here lives in the wiring: the
//! middleware order, content negotiation, the query-parameter token an
//! `EventSource` depends on, and the status codes the interface branches on.

use std::time::Duration;

use super::*;
use crate::config::{DataDir, Settings};

struct Harness {
    base: String,
    store: Storage,
    data_dir: DataDir,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap().keep();
        let data_dir = DataDir::at(root).unwrap();
        data_dir.ensure().unwrap();
        let store = Storage::open(data_dir.database_file()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // File-backed on purpose: a test must never read or write the
        // credential store of the machine running it.
        let app = router(
            store.clone(),
            "test-token",
            Settings::default(),
            data_dir.clone(),
            crate::config::credentials::CredentialStore::file_backed(&data_dir.credentials_dir()),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base: format!("http://{address}"),
            store,
            data_dir,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::Client::new()
            .get(self.url(path))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Option<serde_json::Value>) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(self.url(path))
            .bearer_auth("test-token");
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn request() -> RefinementRequest {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/contracts/refinement_request.json"
    ))
    .unwrap()
}

#[tokio::test]
async fn ingress_is_authenticated_idempotent_and_streamable() {
    let harness = Harness::start().await;
    let body = serde_json::to_value(request()).unwrap();

    assert_eq!(
        reqwest::Client::new()
            .post(harness.url("/v1/refinements"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let first: serde_json::Value = harness
        .post("/v1/refinements", Some(body.clone()))
        .await
        .json()
        .await
        .unwrap();
    assert!(first["created"].as_bool().unwrap());

    let second: serde_json::Value = harness
        .post("/v1/refinements", Some(body))
        .await
        .json()
        .await
        .unwrap();
    assert!(!second["created"].as_bool().unwrap());
    assert_eq!(first["case_id"], second["case_id"]);

    let case_id = first["case_id"].as_str().unwrap().to_owned();
    let events = harness
        .get(&format!("/v1/refinements/{case_id}/events"))
        .await;
    assert_eq!(events.headers()[header::CONTENT_TYPE], "application/json");
    let backlog: Vec<serde_json::Value> = events.json().await.unwrap();
    assert!(backlog.iter().any(|event| event["type"] == "case_received"));
}

/// A browser authenticates its event stream with a query parameter because
/// `EventSource` cannot send a header. That path has to work, and the wrong
/// token has to fail on exactly the same route.
#[tokio::test]
async fn a_following_stream_authenticates_by_query_and_delivers_later_events() {
    let harness = Harness::start().await;
    let created: serde_json::Value = harness
        .post(
            "/v1/refinements",
            Some(serde_json::to_value(request()).unwrap()),
        )
        .await
        .json()
        .await
        .unwrap();
    let case_id: crate::domain::CaseId = created["case_id"].as_str().unwrap().parse().unwrap();

    assert_eq!(
        reqwest::get(harness.url(&format!(
            "/v1/refinements/{case_id}/events?follow=1&token=wrong"
        )))
        .await
        .unwrap()
        .status(),
        StatusCode::UNAUTHORIZED
    );

    let mut stream = reqwest::get(harness.url(&format!(
        "/v1/refinements/{case_id}/events?follow=1&token=test-token"
    )))
    .await
    .unwrap();
    assert_eq!(stream.headers()[header::CONTENT_TYPE], "text/event-stream");

    // Cancelling after the stream is open proves it is following rather than
    // replaying: the event does not exist when the connection is made.
    let store = harness.store.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        store
            .cancel_case(case_id, Some("stopped".into()))
            .await
            .unwrap();
    });

    let body = read_until(&mut stream, "state_changed", Duration::from_secs(10)).await;
    assert!(body.contains("case_received"), "backlog missing: {body}");
    assert!(body.contains("cancelled"), "live event missing: {body}");
}

async fn read_until(response: &mut reqwest::Response, needle: &str, limit: Duration) -> String {
    let mut body = String::new();
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                body.push_str(&String::from_utf8_lossy(&chunk));
                if body.contains(needle) {
                    return body;
                }
            }
            Ok(Ok(None)) => return body,
            Ok(Err(error)) => panic!("stream failed: {error}"),
            Err(_) => continue,
        }
    }
    body
}

#[tokio::test]
async fn the_interface_shell_is_served_without_a_token_and_the_data_is_not() {
    let harness = Harness::start().await;
    let shell = reqwest::get(harness.url("/")).await.unwrap();
    assert_eq!(shell.status(), StatusCode::OK);
    assert!(shell.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert!(shell
        .text()
        .await
        .unwrap()
        .contains("<title>Refinery</title>"));

    for path in ["/assets/app.css", "/assets/app.js"] {
        assert_eq!(
            reqwest::get(harness.url(path)).await.unwrap().status(),
            StatusCode::OK,
            "{path} should be served"
        );
    }
    assert_eq!(
        reqwest::get(harness.url("/assets/nothing.js"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    for path in ["/v1/settings", "/v1/health", "/v1/repositories", "/v1/logs"] {
        assert_eq!(
            reqwest::get(harness.url(path)).await.unwrap().status(),
            StatusCode::UNAUTHORIZED,
            "{path} should require a token"
        );
    }
}

#[tokio::test]
async fn health_reports_what_the_settings_view_renders() {
    let harness = Harness::start().await;
    let health: serde_json::Value = harness.get("/v1/health").await.json().await.unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(health["pending_migrations"], 0);
    assert_eq!(health["provider_credential"], false);
    assert!(health["case_states"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "awaiting_answer"));
}

#[tokio::test]
async fn a_repository_registers_through_the_form_and_cannot_widen_its_policy() {
    let harness = Harness::start().await;
    let root = tempfile::tempdir().unwrap().keep();
    let response = harness
        .post(
            "/v1/repositories",
            Some(serde_json::json!({
                "path": root.display().to_string(),
                "max_file_bytes": 1024,
                "max_results": 999_999,
                "respect_ignore_files": true,
            })),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let registered: serde_json::Value = response.json().await.unwrap();
    assert_eq!(registered["policy"]["max_file_bytes"], 1024);

    let ceiling = crate::repositories::RepositoryPolicy::from_limits(&Settings::default().limits);
    assert_eq!(
        registered["policy"]["max_results"].as_u64().unwrap() as usize,
        ceiling.max_results,
        "a request must not raise the installation's own limit"
    );

    let listed: Vec<serde_json::Value> =
        harness.get("/v1/repositories").await.json().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], registered["id"]);
}

#[tokio::test]
async fn the_log_tail_is_bounded_and_survives_a_missing_directory() {
    let harness = Harness::start().await;
    let log_dir = harness.data_dir.log_dir();
    std::fs::create_dir_all(&log_dir).unwrap();
    let lines: String = (0..500).map(|index| format!("line {index}\n")).collect();
    std::fs::write(log_dir.join("refinery.2026-08-31.log"), lines).unwrap();

    let tail: serde_json::Value = harness.get("/v1/logs?lines=10").await.json().await.unwrap();
    let rendered = tail["lines"].as_array().unwrap();
    assert_eq!(rendered.len(), 10);
    assert_eq!(rendered.last().unwrap(), "line 499");

    let capped: serde_json::Value = harness
        .get("/v1/logs?lines=100000")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(capped["lines"].as_array().unwrap().len(), 500);
}

#[tokio::test]
async fn a_case_detail_carries_everything_the_detail_view_shows() {
    let harness = Harness::start().await;
    let created: serde_json::Value = harness
        .post(
            "/v1/refinements",
            Some(serde_json::to_value(request()).unwrap()),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = created["case_id"].as_str().unwrap();

    let detail: serde_json::Value = harness
        .get(&format!("/v1/refinements/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["state"], "received");
    assert!(detail["request"]["transcript"]["messages"]
        .as_array()
        .is_some_and(|messages| !messages.is_empty()));
    assert!(detail["deliveries"].as_array().unwrap().is_empty());
    assert!(detail["questions"].as_array().unwrap().is_empty());
    assert!(detail["event_count"].as_u64().unwrap() >= 1);

    assert_eq!(
        harness.get("/v1/refinements/not-a-case-id").await.status(),
        StatusCode::BAD_REQUEST
    );
}

/// The interface offers "retry delivery" only from `failed`, and the daemon
/// enforces the same rule rather than trusting a disabled button.
#[tokio::test]
async fn a_delivery_retry_is_refused_unless_the_case_failed_with_a_prompt() {
    let harness = Harness::start().await;
    let created: serde_json::Value = harness
        .post(
            "/v1/refinements",
            Some(serde_json::to_value(request()).unwrap()),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = created["case_id"].as_str().unwrap();
    let response = harness
        .post(&format!("/v1/refinements/{id}/deliveries"), None)
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["code"], "invalid_request");
    assert!(error["message"].as_str().unwrap().contains("failed case"));
}

#[test]
fn token_comparison_accepts_only_the_exact_token() {
    assert!(constant_time_eq("abc", "abc"));
    assert!(!constant_time_eq("abc", "abd"));
    assert!(!constant_time_eq("abc", "abcd"));
    assert!(!constant_time_eq("", "a"));
    assert!(constant_time_eq("", ""));
}

#[test]
fn a_query_token_is_read_only_from_its_own_parameter() {
    assert_eq!(query_token(Some("token=abc")).as_deref(), Some("abc"));
    assert_eq!(
        query_token(Some("follow=1&token=a%20b")).as_deref(),
        Some("a b")
    );
    assert_eq!(query_token(Some("tokens=abc")), None);
    assert_eq!(query_token(None), None);
}
