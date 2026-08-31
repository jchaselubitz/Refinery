//! The provider connection test.
//!
//! Setup and `doctor` both need to answer one question: does the stored
//! credential actually work against the configured model? The check is a
//! single authenticated metadata read — the cheapest call that distinguishes
//! "the key is wrong" from "the model name is wrong" from "the network is
//! down", each of which has a different remedy.
//!
//! The key travels in the `x-goog-api-key` header rather than a `key=` query
//! parameter, so it never reaches a URL that a client, proxy, or error message
//! might echo. Nothing here returns the secret, and every failure message is
//! passed through the redaction layer before it is surfaced, because a
//! provider's own error body is one of the few places a key can come back to
//! us uninvited.

use std::time::Duration;

use serde::Deserialize;

use crate::diagnostics::redaction;
use crate::domain::secret::SecretString;
use crate::error::{AppError, Result, RetryClass};

/// The default Gemini API base. Overridable so tests can point at a local
/// server without any network access.
pub const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// How long a connection test waits before reporting the provider unreachable.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// What a successful connection test learned about the model.
///
/// This is deliberately small: setup prints it to reassure the user that the
/// configured model is real, and `doctor` reports it as evidence the
/// credential works. Full capability negotiation belongs to M6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConnection {
    /// The provider's canonical name for the model, for example
    /// `models/gemini-3.7-flash`.
    pub model: String,
    /// The model's human-readable display name, when the provider gives one.
    pub display_name: Option<String>,
    /// The largest input the model accepts, in tokens, when reported.
    pub input_token_limit: Option<u64>,
    /// The largest output the model produces, in tokens, when reported.
    pub output_token_limit: Option<u64>,
}

impl ProviderConnection {
    /// A one-line description for setup and `doctor` output.
    pub fn summary(&self) -> String {
        let name = self.display_name.as_deref().unwrap_or(&self.model);
        match self.input_token_limit {
            Some(limit) => format!("{name} ({} input tokens)", thousands(limit)),
            None => name.to_owned(),
        }
    }
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Authenticate against the provider and read metadata for one model.
///
/// The secret is never printed, returned, or placed in the URL.
pub async fn test_gemini(
    base_url: &str,
    model: &str,
    api_key: &SecretString,
) -> Result<ProviderConnection> {
    let client = reqwest::Client::builder()
        .timeout(TEST_TIMEOUT)
        .build()
        .map_err(|source| {
            upstream(
                format!("could not build an HTTP client: {source}"),
                RetryClass::NonRetryable,
            )
        })?;

    let url = format!(
        "{}/models/{}",
        base_url.trim_end_matches('/'),
        model.trim_start_matches("models/")
    );

    let response = client
        .get(&url)
        .header("x-goog-api-key", api_key.expose())
        .send()
        .await
        .map_err(|source| {
            // A transport failure is the network, not the credential, so it is
            // retryable and its remedy is different.
            upstream(
                format!("could not reach the provider: {source}"),
                RetryClass::Retryable,
            )
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(classify_failure(status, &body));
    }

    let model: ModelMetadata = serde_json::from_str(&body).map_err(|source| {
        upstream(
            format!("the provider returned an unreadable model description: {source}"),
            RetryClass::NonRetryable,
        )
    })?;

    Ok(ProviderConnection {
        model: model.name,
        display_name: model.display_name,
        input_token_limit: model.input_token_limit,
        output_token_limit: model.output_token_limit,
    })
}

/// Turn a provider failure into an error whose classification names the remedy.
///
/// This is shared with the Files API client in [`crate::media`]: every Gemini
/// surface must agree on which status codes mean "fix your key", "try again",
/// and "this will never work", because the job runner acts on that
/// classification and never on the message text.
///
/// The distinction matters to both callers: `doctor` prints a different fix for
/// a rejected key than for a missing model, and the job runner in later
/// milestones retries only what can succeed on a second attempt.
pub(crate) fn classify_failure(status: reqwest::StatusCode, body: &str) -> AppError {
    let detail = provider_message(body);
    match status.as_u16() {
        400 | 401 | 403 => AppError::NeedsUser {
            message: redaction::redact(&format!(
                "the provider rejected the credential ({status}): {detail}"
            ))
            .into_owned(),
        },
        404 => upstream(
            format!("the provider does not offer this model ({status}): {detail}"),
            RetryClass::NonRetryable,
        ),
        429 | 500..=599 => upstream(
            format!("the provider is temporarily unavailable ({status}): {detail}"),
            RetryClass::Retryable,
        ),
        _ => upstream(
            format!("the provider returned {status}: {detail}"),
            RetryClass::NonRetryable,
        ),
    }
}

/// Pull the human-readable part out of a provider error body, falling back to
/// a bounded slice of the raw body when it is not the expected shape.
pub(crate) fn provider_message(body: &str) -> String {
    let message = serde_json::from_str::<ErrorEnvelope>(body)
        .ok()
        .map(|envelope| envelope.error.message)
        .unwrap_or_else(|| body.trim().to_owned());

    let mut message = message;
    // The body is untrusted provider output; bound it so a large error page
    // cannot flood a terminal or a log line.
    const MAX: usize = 300;
    if message.len() > MAX {
        message.truncate(
            (0..=MAX)
                .rev()
                .find(|&index| message.is_char_boundary(index))
                .unwrap_or(0),
        );
        message.push('…');
    }
    if message.is_empty() {
        message.push_str("no detail given");
    }
    message
}

pub(crate) fn upstream(message: String, retry: RetryClass) -> AppError {
    AppError::Upstream {
        service: "gemini",
        message: redaction::redact(&message).into_owned(),
        retry,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelMetadata {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    output_token_limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ProviderError,
}

#[derive(Debug, Deserialize)]
struct ProviderError {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejected_credential_asks_the_user_to_fix_it() {
        let error = classify_failure(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":{"message":"API key not valid"}}"#,
        );
        assert_eq!(error.retry_class(), RetryClass::NeedsUser);
        assert!(error.to_string().contains("API key not valid"));
    }

    #[test]
    fn a_missing_model_is_permanent_and_a_rate_limit_is_not() {
        assert_eq!(
            classify_failure(reqwest::StatusCode::NOT_FOUND, "{}").retry_class(),
            RetryClass::NonRetryable
        );
        assert_eq!(
            classify_failure(reqwest::StatusCode::TOO_MANY_REQUESTS, "{}").retry_class(),
            RetryClass::Retryable
        );
        assert_eq!(
            classify_failure(reqwest::StatusCode::BAD_GATEWAY, "{}").retry_class(),
            RetryClass::Retryable
        );
    }

    #[test]
    fn a_provider_error_echoing_the_key_back_is_redacted() {
        // Google's own 400 body quotes the offending key. This is exactly the
        // case the registry cannot cover, since the failure text is built from
        // provider output.
        let error = classify_failure(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"API key not valid: AIzaSyD-ExampleExampleExampleExample1"}}"#,
        );
        assert!(!error.to_string().contains("AIzaSyD"));
    }

    #[test]
    fn an_oversized_error_body_is_bounded() {
        let body = "x".repeat(10_000);
        let message = provider_message(&body);
        assert!(
            message.len() <= 320,
            "unbounded provider text: {}",
            message.len()
        );
    }

    #[test]
    fn a_multibyte_error_body_is_truncated_on_a_character_boundary() {
        let body = "é".repeat(1_000);
        let message = provider_message(&body);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn an_empty_body_still_says_something() {
        assert_eq!(provider_message("   "), "no detail given");
    }

    #[test]
    fn the_summary_reads_as_a_sentence() {
        let connection = ProviderConnection {
            model: "models/gemini-3.7-flash".into(),
            display_name: Some("Gemini 3.7 Flash".into()),
            input_token_limit: Some(1_048_576),
            output_token_limit: Some(65_536),
        };
        assert_eq!(
            connection.summary(),
            "Gemini 3.7 Flash (1,048,576 input tokens)"
        );
    }

    #[test]
    fn the_summary_falls_back_to_the_model_name() {
        let connection = ProviderConnection {
            model: "models/gemini-3.7-flash".into(),
            display_name: None,
            input_token_limit: None,
            output_token_limit: None,
        };
        assert_eq!(connection.summary(), "models/gemini-3.7-flash");
    }

    #[test]
    fn model_metadata_parses_the_provider_shape() {
        let metadata: ModelMetadata = serde_json::from_str(
            r#"{"name":"models/gemini-3.7-flash","displayName":"Gemini 3.7 Flash",
                "inputTokenLimit":1048576,"outputTokenLimit":65536,"supportedGenerationMethods":["generateContent"]}"#,
        )
        .expect("parse");
        assert_eq!(metadata.name, "models/gemini-3.7-flash");
        assert_eq!(metadata.input_token_limit, Some(1_048_576));
        assert_eq!(metadata.output_token_limit, Some(65_536));
    }
}
