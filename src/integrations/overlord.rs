//! The Overlord destination adapter and question callback.
//!
//! A local Overlord instance submits over loopback and receives questions and
//! results through its authenticated local endpoint, so this path needs no
//! cloud component. A hosted Overlord would consume an outbound rendezvous
//! channel owned by Overlord; Refinery never exposes a public listener.
//! The HTTP boundary is deliberately narrow: Refinery supplies only a
//! versioned envelope, a stable idempotency header, and an optional bearer
//! credential. Content from either side is data, never executable input.

use async_trait::async_trait;
use reqwest::StatusCode;

use crate::{
    domain::{DeliveryEnvelope, DeliveryReceipt, QuestionRequest, SecretString, SourceCallback},
    error::{AppError, Result, RetryClass},
};

/// A delivery destination. Keeping it as an interface makes local export and
/// future rendezvous transports use exactly the same retry and audit path.
#[async_trait]
pub trait DestinationAdapter: Send + Sync {
    /// Submit one idempotent delivery envelope.
    async fn submit(&self, envelope: &DeliveryEnvelope) -> Result<DeliveryReceipt>;
}

/// The local loopback Overlord HTTP adapter.
#[derive(Clone, Debug, Default)]
pub struct LocalOverlordAdapter {
    client: reqwest::Client,
    base_url: String,
    bearer_token: Option<SecretString>,
}

impl LocalOverlordAdapter {
    /// Construct the adapter with the shared Rustls HTTP client.
    pub fn new(base_url: impl Into<String>, bearer_token: Option<SecretString>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            bearer_token,
        }
    }

    /// Notify the source that a question is waiting. This uses the callback
    /// supplied in the accepted request, never a discovered remote address.
    pub async fn forward_question(
        &self,
        callback: &SourceCallback,
        question: &QuestionRequest,
    ) -> Result<()> {
        let mut request = self.client.post(&callback.questions_url).json(question);
        if let Some(token) = &callback.bearer_token {
            request = request.bearer_auth(token.expose());
        }
        let response = request.send().await.map_err(network_error)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(status_error(response.status()))
        }
    }
}

#[async_trait]
impl DestinationAdapter for LocalOverlordAdapter {
    async fn submit(&self, envelope: &DeliveryEnvelope) -> Result<DeliveryReceipt> {
        // The endpoint is intentionally fixed beneath the configured local
        // base URL, so the destination contract cannot smuggle a path with a
        // different authority or semantics.
        let url = format!(
            "{}/v1/refinery/deliveries",
            self.base_url.trim_end_matches('/')
        );
        let mut request = self
            .client
            .post(url)
            .header("Idempotency-Key", &envelope.idempotency_key)
            .json(envelope);
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token.expose());
        }
        let response = request.send().await.map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(status));
        }
        response
            .json::<DeliveryReceipt>()
            .await
            .map_err(|error| AppError::Upstream {
                service: "overlord",
                message: format!("invalid delivery receipt: {error}"),
                retry: RetryClass::Retryable,
            })
    }
}

fn network_error(error: reqwest::Error) -> AppError {
    AppError::Upstream {
        service: "overlord",
        message: error.to_string(),
        retry: RetryClass::Retryable,
    }
}
fn status_error(status: StatusCode) -> AppError {
    let retry = if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        RetryClass::Retryable
    } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        RetryClass::NeedsUser
    } else {
        RetryClass::NonRetryable
    };
    AppError::Upstream {
        service: "overlord",
        message: format!("returned HTTP {status}"),
        retry,
    }
}
