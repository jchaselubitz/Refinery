//! The Gemini Files API client.
//!
//! Gemini will not accept image or video bytes inline at the sizes Refinery
//! cares about, so media is uploaded once and referenced by the provider's own
//! file name for as long as the provider keeps it. That upload is a resumable
//! two-step exchange: a start request that declares the length and type and
//! returns a one-shot upload URL, then a finalize request carrying the bytes.
//!
//! Video is not usable the moment it is uploaded. The provider transcodes it,
//! and the file sits in `PROCESSING` until that finishes, so this client polls
//! to activation rather than handing the agent loop a file the model would
//! refuse. Polling is bounded: a file that never activates is a provider
//! failure, not a case that waits forever.
//!
//! Every uploaded file carries an expiry — roughly two days — after which the
//! provider deletes it. That expiry is returned here and persisted by the
//! lifecycle layer, because Refinery's answer to expiry is to upload again
//! from the local media store, and it can only do that if it knows when.
//!
//! The API key travels in the `x-goog-api-key` header, never in a URL, and
//! every provider message passes through the redaction layer before it can
//! reach a log or an error body.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::agent::connection_test::{classify_failure, upstream};
use crate::diagnostics::redaction;
use crate::domain::secret::SecretString;
use crate::error::{AppError, Result, RetryClass};

/// The Files API lives under the same base as the model API.
pub const GEMINI_FILES_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a provider copy is assumed to live when the provider does not say.
///
/// The documented lifetime is 48 hours. Assuming slightly less means Refinery
/// re-uploads a little early rather than handing the model a file that has
/// just been collected, which is the cheaper of the two mistakes.
const ASSUMED_LIFETIME_HOURS: i64 = 47;

/// How the client waits for a video to finish processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationPolicy {
    /// Delay between activation checks.
    pub interval: Duration,
    /// Total time to wait before declaring the provider stuck.
    pub max_wait: Duration,
}

impl Default for ActivationPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            max_wait: Duration::from_secs(300),
        }
    }
}

/// Where a provider-held file stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFileState {
    /// Uploaded and still being prepared; not yet referenceable.
    Processing,
    /// Ready for the model to reference.
    Active,
    /// The provider gave up on the file.
    Failed,
}

impl ProviderFileState {
    fn parse(value: &str) -> Self {
        match value {
            "ACTIVE" => ProviderFileState::Active,
            "FAILED" => ProviderFileState::Failed,
            _ => ProviderFileState::Processing,
        }
    }
}

/// A file the provider is holding on Refinery's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFile {
    /// The provider's resource name, for example `files/abc123`. This is what
    /// the agent loop references and what Refinery persists.
    pub name: String,
    /// The URI the model uses to read the file.
    pub uri: String,
    /// The provider's declared media type for the stored copy.
    pub media_type: String,
    /// Whether the file is usable yet.
    pub state: ProviderFileState,
    /// When the provider will delete the file.
    pub expires_at: DateTime<Utc>,
}

/// A client for one Gemini project's Files API.
#[derive(Clone)]
pub struct GeminiFilesClient {
    http: reqwest::Client,
    base_url: String,
    api_key: SecretString,
    activation: ActivationPolicy,
}

impl std::fmt::Debug for GeminiFilesClient {
    /// The API key is deliberately absent: a client is held by long-lived
    /// structures that end up in debug output, and `SecretString` redacting
    /// itself is only half the protection if the field is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiFilesClient")
            .field("base_url", &self.base_url)
            .field("activation", &self.activation)
            .finish_non_exhaustive()
    }
}

impl GeminiFilesClient {
    /// Build a client against a base URL, so tests can point at a local server
    /// and never reach the network.
    pub fn new(base_url: impl Into<String>, api_key: SecretString) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(GEMINI_FILES_TIMEOUT)
            .build()
            .map_err(|source| {
                upstream(
                    format!("could not build an HTTP client: {source}"),
                    RetryClass::NonRetryable,
                )
            })?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
            activation: ActivationPolicy::default(),
        })
    }

    /// Replace the activation-polling policy. Tests use a fast one.
    pub fn with_activation_policy(mut self, activation: ActivationPolicy) -> Self {
        self.activation = activation;
        self
    }

    /// Upload bytes and return as soon as the provider accepts them.
    ///
    /// The returned file may still be `Processing`; callers that need a usable
    /// file should use [`GeminiFilesClient::upload_and_activate`].
    pub async fn upload(
        &self,
        display_name: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<ProviderFile> {
        let upload_url = self
            .begin_upload(display_name, media_type, bytes.len())
            .await?;
        self.finalize_upload(&upload_url, bytes).await
    }

    /// Upload bytes and wait until the provider reports the file usable.
    pub async fn upload_and_activate(
        &self,
        display_name: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<ProviderFile> {
        let file = self.upload(display_name, media_type, bytes).await?;
        self.await_active(file).await
    }

    /// Poll a file until it is active, it fails, or the wait budget runs out.
    pub async fn await_active(&self, file: ProviderFile) -> Result<ProviderFile> {
        let mut current = file;
        let deadline = std::time::Instant::now() + self.activation.max_wait;
        loop {
            match current.state {
                ProviderFileState::Active => return Ok(current),
                ProviderFileState::Failed => {
                    // A file the provider gave up on will never activate, but
                    // a fresh upload of the same bytes usually succeeds, so
                    // this is retryable at the job level rather than fatal.
                    return Err(upstream(
                        format!("the provider could not process {}", current.name),
                        RetryClass::Retryable,
                    ));
                }
                ProviderFileState::Processing => {
                    if std::time::Instant::now() >= deadline {
                        return Err(upstream(
                            format!(
                                "{} was still processing after {} seconds",
                                current.name,
                                self.activation.max_wait.as_secs()
                            ),
                            RetryClass::Retryable,
                        ));
                    }
                    tokio::time::sleep(self.activation.interval).await;
                    current = self.get(&current.name).await?;
                }
            }
        }
    }

    /// Read the provider's current record for one file.
    pub async fn get(&self, name: &str) -> Result<ProviderFile> {
        let response = self
            .http
            .get(self.file_url(name))
            .header("x-goog-api-key", self.api_key.expose())
            .send()
            .await
            .map_err(transport_failure)?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(classify_failure(status, &body));
        }
        parse_file(&body)
    }

    /// Delete a provider copy. A file that is already gone is a success:
    /// deletion is only ever used to clean up, and the desired end state is
    /// the same either way.
    pub async fn delete(&self, name: &str) -> Result<()> {
        let response = self
            .http
            .delete(self.file_url(name))
            .header("x-goog-api-key", self.api_key.expose())
            .send()
            .await
            .map_err(transport_failure)?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(classify_failure(status, &body))
    }

    /// Step one of the resumable upload: declare the length and type, and
    /// receive a one-shot URL to send the bytes to.
    async fn begin_upload(
        &self,
        display_name: &str,
        media_type: &str,
        length: usize,
    ) -> Result<String> {
        let response = self
            .http
            .post(format!("{}/files", self.base_url))
            .header("x-goog-api-key", self.api_key.expose())
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header("X-Goog-Upload-Header-Content-Length", length.to_string())
            .header("X-Goog-Upload-Header-Content-Type", media_type)
            .json(&serde_json::json!({ "file": { "display_name": display_name } }))
            .send()
            .await
            .map_err(transport_failure)?;

        let status = response.status();
        let upload_url = response
            .headers()
            .get("x-goog-upload-url")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(classify_failure(status, &body));
        }
        upload_url.ok_or_else(|| {
            upstream(
                "the provider accepted the upload request without returning an upload URL"
                    .to_owned(),
                RetryClass::Retryable,
            )
        })
    }

    /// Step two: send the bytes and finalize in one request.
    async fn finalize_upload(&self, upload_url: &str, bytes: Vec<u8>) -> Result<ProviderFile> {
        let length = bytes.len();
        let response = self
            .http
            .post(upload_url)
            .header("x-goog-api-key", self.api_key.expose())
            .header("Content-Length", length.to_string())
            .header("X-Goog-Upload-Offset", "0")
            .header("X-Goog-Upload-Command", "upload, finalize")
            .body(bytes)
            .send()
            .await
            .map_err(transport_failure)?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(classify_failure(status, &body));
        }
        parse_file(&body)
    }

    /// The provider addresses files as `files/<id>`; accept either spelling
    /// from a caller so a persisted name round-trips without reformatting.
    fn file_url(&self, name: &str) -> String {
        format!(
            "{}/files/{}",
            self.base_url,
            name.trim_start_matches("files/")
        )
    }
}

/// A transport failure is the network, not the credential, so it is retryable
/// and its remedy differs from a rejected request.
fn transport_failure(source: reqwest::Error) -> AppError {
    upstream(
        redaction::redact(&format!("could not reach the provider: {source}")).into_owned(),
        RetryClass::Retryable,
    )
}

/// Read the provider's file description out of an upload or get response.
fn parse_file(body: &str) -> Result<ProviderFile> {
    let envelope: FileEnvelope = serde_json::from_str(body).map_err(|source| {
        upstream(
            format!("the provider returned an unreadable file description: {source}"),
            RetryClass::Retryable,
        )
    })?;
    // Both `POST /files` and `GET /files/{id}` are accepted: the first wraps
    // the file under a `file` key and the second does not.
    let file = envelope.file.or(envelope.flat).ok_or_else(|| {
        upstream(
            "the provider returned a response with no file in it".to_owned(),
            RetryClass::Retryable,
        )
    })?;
    if file.name.is_empty() {
        return Err(upstream(
            "the provider returned a file with no name".to_owned(),
            RetryClass::Retryable,
        ));
    }
    Ok(ProviderFile {
        uri: file.uri.unwrap_or_else(|| file.name.clone()),
        media_type: file
            .mime_type
            .unwrap_or_else(|| "application/octet-stream".to_owned()),
        state: ProviderFileState::parse(file.state.as_deref().unwrap_or("PROCESSING")),
        expires_at: file
            .expiration_time
            .unwrap_or_else(|| Utc::now() + chrono::Duration::hours(ASSUMED_LIFETIME_HOURS)),
        name: file.name,
    })
}

#[derive(Debug, Deserialize)]
struct FileEnvelope {
    #[serde(default)]
    file: Option<FileBody>,
    #[serde(flatten, default)]
    flat: Option<FileBody>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    uri: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    expiration_time: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upload_response_is_read_out_of_its_file_wrapper() {
        let file = parse_file(
            r#"{"file":{"name":"files/abc","uri":"https://example/files/abc","mimeType":"video/mp4","state":"PROCESSING","expirationTime":"2030-01-02T03:04:05Z"}}"#,
        )
        .unwrap();
        assert_eq!(file.name, "files/abc");
        assert_eq!(file.media_type, "video/mp4");
        assert_eq!(file.state, ProviderFileState::Processing);
        assert_eq!(file.expires_at.to_rfc3339(), "2030-01-02T03:04:05+00:00");
    }

    #[test]
    fn a_get_response_without_the_wrapper_reads_the_same_way() {
        let file =
            parse_file(r#"{"name":"files/abc","uri":"u","mimeType":"image/png","state":"ACTIVE"}"#)
                .unwrap();
        assert_eq!(file.state, ProviderFileState::Active);
        assert_eq!(file.name, "files/abc");
    }

    #[test]
    fn a_missing_expiry_falls_back_to_the_documented_lifetime() {
        let before = Utc::now();
        let file = parse_file(r#"{"name":"files/abc","state":"ACTIVE"}"#).unwrap();
        assert!(file.expires_at > before + chrono::Duration::hours(ASSUMED_LIFETIME_HOURS - 1));
        assert!(file.expires_at <= Utc::now() + chrono::Duration::hours(ASSUMED_LIFETIME_HOURS));
    }

    #[test]
    fn a_response_with_no_usable_file_is_a_retryable_provider_error() {
        let error = parse_file("{}").unwrap_err();
        assert_eq!(error.retry_class(), RetryClass::Retryable);
        assert!(parse_file("not json").is_err());
        assert!(parse_file(r#"{"file":{"name":""}}"#).is_err());
    }

    #[test]
    fn an_unknown_provider_state_is_treated_as_still_processing() {
        assert_eq!(
            ProviderFileState::parse("SOMETHING_NEW"),
            ProviderFileState::Processing
        );
    }

    #[test]
    fn a_persisted_file_name_round_trips_into_a_url() {
        let client = GeminiFilesClient::new(
            "https://example.test/v1beta/",
            SecretString::new("key".to_owned()),
        )
        .unwrap();
        assert_eq!(
            client.file_url("files/abc"),
            "https://example.test/v1beta/files/abc"
        );
        assert_eq!(
            client.file_url("abc"),
            "https://example.test/v1beta/files/abc"
        );
    }

    #[test]
    fn the_client_never_prints_its_key() {
        let client = GeminiFilesClient::new(
            "https://example.test",
            SecretString::new("super-secret-key".to_owned()),
        )
        .unwrap();
        assert!(!format!("{client:?}").contains("super-secret-key"));
    }
}
