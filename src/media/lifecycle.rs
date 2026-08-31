//! Moving attachments from a submitted request to something a model can read.
//!
//! Two operations live here, and the second is the reason the first exists.
//!
//! [`import_request_attachments`] copies every submitted attachment into the
//! case's media directory, recording each as `imported` or, when the bytes
//! contradict the declaration, as `rejected` with a reason. A rejected
//! attachment does not fail the case: the transcript and the other inputs are
//! still worth refining, and the refusal is on the record for whoever asks.
//!
//! [`ensure_available`] is what the agent loop calls before referencing an
//! attachment. It uploads on first use, and it re-uploads when the provider's
//! copy has expired. Expiry is normal — Gemini keeps files about two days,
//! and a case that waits overnight for a human answer will routinely come
//! back to expired uploads — so it is handled silently from the local bytes
//! rather than surfaced as a failure. That is the whole justification for
//! keeping the media store: the provider's copy is a cache, and Refinery owns
//! the original.
//!
//! The state sequence is `imported` → `uploading` → `available` → `expired` →
//! `uploading` → `available`, with `rejected` reachable only from import.
//! Every transition is a durable, evented storage write, so a crash mid-upload
//! leaves an attachment in `uploading` that recovery can drive forward: the
//! bytes and the digest are still on disk, and a second upload of the same
//! bytes is harmless.

use chrono::{DateTime, Utc};

use crate::domain::{AttachmentInput, AttachmentMetadata, AttachmentState, CaseId};
use crate::error::{AppError, Result};
use crate::media::files_api::{GeminiFilesClient, ProviderFile};
use crate::media::store::MediaStore;
use crate::storage::{AttachmentStateUpdate, Storage, StoredAttachment};

/// What importing one request's attachments produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    /// Attachments now holding verified bytes in the media store.
    pub imported: Vec<AttachmentMetadata>,
    /// Attachments refused, each carrying the reason on its metadata.
    pub rejected: Vec<AttachmentMetadata>,
}

impl ImportSummary {
    /// Whether every submitted attachment was accepted.
    pub fn is_clean(&self) -> bool {
        self.rejected.is_empty()
    }
}

/// Import every attachment on a request into the case's media directory.
///
/// Import never fails the case. A refusal is recorded against the attachment
/// and the remaining inputs continue, because a case is mostly a transcript
/// and one bad screenshot is not a reason to throw it away.
pub async fn import_request_attachments(
    storage: &Storage,
    store: &MediaStore,
    case_id: CaseId,
    attachments: &[AttachmentInput],
) -> Result<ImportSummary> {
    let mut summary = ImportSummary::default();
    for input in attachments {
        match store.import(case_id, input) {
            Ok(imported) => {
                storage
                    .record_imported_attachment(case_id, &imported)
                    .await?;
                summary.imported.push(imported.metadata);
            }
            Err(rejection) => {
                let id = storage
                    .record_rejected_attachment(case_id, input, &rejection)
                    .await?;
                let stored = storage.attachment(id).await?;
                tracing::warn!(
                    case_id = %case_id,
                    attachment_id = %id,
                    code = rejection.code.as_str(),
                    "attachment rejected at import"
                );
                summary.rejected.push(stored.metadata);
            }
        }
    }
    Ok(summary)
}

/// Ensure the provider holds a usable copy of one attachment, uploading or
/// re-uploading from the local media store when it does not.
///
/// Returns the provider's file name, which is what the agent loop puts in a
/// request. The `now` argument is passed in rather than read here so the
/// expiry rule is testable without waiting two days.
pub async fn ensure_available(
    storage: &Storage,
    store: &MediaStore,
    files: &GeminiFilesClient,
    stored: &StoredAttachment,
    now: DateTime<Utc>,
) -> Result<AttachmentMetadata> {
    if stored.metadata.state == AttachmentState::Rejected {
        return Err(AppError::PolicyDenied {
            message: format!(
                "attachment {} was rejected at import and will not be uploaded",
                stored.metadata.id
            ),
        });
    }

    // A live provider copy is used as is. The clock decides, not the recorded
    // state: an attachment can sit in `available` past its expiry simply
    // because nothing has looked at it since.
    if stored.metadata.state == AttachmentState::Available
        && !stored.metadata.is_expired_at(now)
        && stored.metadata.provider_file_name.is_some()
    {
        return Ok(stored.metadata.clone());
    }

    if stored.metadata.is_expired_at(now) && stored.metadata.state != AttachmentState::Expired {
        // Record the observation before acting on it, so the history explains
        // why a second upload of the same bytes happened.
        storage
            .record_attachment_state(stored.metadata.id, AttachmentStateUpdate::expired())
            .await?;
    }

    let path = stored
        .media_path
        .as_ref()
        .ok_or_else(|| AppError::Invalid {
            message: format!(
                "attachment {} has no local bytes to upload",
                stored.metadata.id
            ),
        })?;
    let bytes = store.read_verified(path, &stored.metadata)?;

    storage
        .record_attachment_state(stored.metadata.id, AttachmentStateUpdate::uploading())
        .await?;

    let uploaded = match files
        .upload_and_activate(&stored.metadata.name, &stored.metadata.media_type, bytes)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            // Leave the attachment needing upload rather than stuck mid-flight,
            // so a retry — by the job runner or by the next use — starts from a
            // state that says exactly what has to happen.
            storage
                .record_attachment_state(stored.metadata.id, AttachmentStateUpdate::upload_failed())
                .await?;
            return Err(error);
        }
    };

    let ProviderFile {
        name, expires_at, ..
    } = uploaded;
    let updated = storage
        .record_attachment_state(
            stored.metadata.id,
            AttachmentStateUpdate::available(name, now, expires_at),
        )
        .await?;
    Ok(updated.metadata)
}

/// Ensure every non-rejected attachment on a case has a usable provider copy.
///
/// Returns the metadata for each usable attachment in import order. A rejected
/// attachment is skipped rather than treated as an error, since the decision
/// to continue without it was already made and recorded at import.
pub async fn ensure_case_attachments_available(
    storage: &Storage,
    store: &MediaStore,
    files: &GeminiFilesClient,
    case_id: CaseId,
    now: DateTime<Utc>,
) -> Result<Vec<AttachmentMetadata>> {
    let mut available = Vec::new();
    for stored in storage.case_attachments(case_id).await? {
        if stored.metadata.state == AttachmentState::Rejected {
            continue;
        }
        available.push(ensure_available(storage, store, files, &stored, now).await?);
    }
    Ok(available)
}

/// The current wall clock, wrapped so callers that have no reason to reach for
/// `chrono` directly do not have to.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}
