//! The local-export destination: write the refined prompt to a file.
//!
//! It exists so that "where does the prompt go" is answered by the same
//! interface whether the answer is Overlord or a path on disk. Retry
//! classification, per-attempt idempotency keys, and the recorded delivery row
//! are then identical for both, and the case state machine never learns that
//! one destination speaks HTTP and the other does not.
//!
//! The write is atomic: the envelope goes to a temporary file beside the
//! destination and is renamed into place. A retry of an attempt that crashed
//! halfway therefore never leaves a destination holding half a prompt, which
//! is the same promise the HTTP adapter gets from its idempotency key.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{
    domain::{DeliveryEnvelope, DeliveryReceipt, SchemaVersion},
    error::{AppError, Result},
};

/// Writes one delivery envelope to an absolute local path.
#[derive(Clone, Debug)]
pub struct LocalExportAdapter {
    path: PathBuf,
}

impl LocalExportAdapter {
    /// Bind the adapter to the destination path the request named.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl super::overlord::DestinationAdapter for LocalExportAdapter {
    async fn submit(&self, envelope: &DeliveryEnvelope) -> Result<DeliveryReceipt> {
        if !self.path.is_absolute() {
            return Err(AppError::Invalid {
                message: format!(
                    "a local export destination must be an absolute path, found {}",
                    self.path.display()
                ),
            });
        }
        let body = serde_json::to_vec_pretty(envelope)
            .map_err(|error| AppError::Internal(error.into()))?;
        write_atomically(&self.path, &body).await?;
        crate::config::paths::set_private_file_mode(&self.path)?;
        Ok(DeliveryReceipt {
            schema_version: SchemaVersion::CURRENT,
            accepted: true,
            destination_reference: Some(self.path.display().to_string()),
            received_at: Some(chrono::Utc::now()),
            message: None,
        })
    }
}

async fn write_atomically(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| AppError::Invalid {
        message: format!("{} has no parent directory", path.display()),
    })?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|source| AppError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    let temporary = path.with_extension(format!(
        "{}.partial",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("json")
    ));
    tokio::fs::write(&temporary, body)
        .await
        .map_err(|source| AppError::Io {
            path: temporary.clone(),
            source,
        })?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|source| AppError::Io {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RetryClass;
    use crate::integrations::overlord::DestinationAdapter;

    fn envelope() -> DeliveryEnvelope {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/delivery_envelope.json"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn an_export_writes_the_envelope_and_reports_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("prompt.json");
        let receipt = LocalExportAdapter::new(&path)
            .submit(&envelope())
            .await
            .unwrap();
        assert!(receipt.accepted);
        assert_eq!(
            receipt.destination_reference.as_deref(),
            Some(path.display().to_string().as_str())
        );
        let written: DeliveryEnvelope =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written, envelope());
        assert!(!dir
            .path()
            .join("nested")
            .join("prompt.json.partial")
            .exists());
    }

    #[tokio::test]
    async fn a_relative_destination_is_refused_rather_than_resolved() {
        let error = LocalExportAdapter::new("prompt.json")
            .submit(&envelope())
            .await
            .unwrap_err();
        assert_eq!(error.retry_class(), RetryClass::NonRetryable);
    }
}
