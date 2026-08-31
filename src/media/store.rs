//! The case-scoped local media store.
//!
//! Every attachment Refinery accepts is copied once, at import, into a
//! directory owned by its case. Nothing later in the pipeline reads the
//! submitter's original path: a `local_path` attachment is a one-time read,
//! not a reference, so a file that changes or disappears after submission
//! cannot change what the case was about.
//!
//! Holding the bytes locally is also what makes provider expiry survivable.
//! Gemini deletes uploaded files after about two days; because the store still
//! has them, an expired upload is re-uploaded instead of failing the case.
//!
//! Import is the trust boundary. The submitter's declared media type, size,
//! and digest are treated as claims to be checked against the bytes, and a
//! claim that does not hold rejects the attachment rather than being silently
//! corrected — a case that says it is sending a screenshot and is in fact
//! sending something else is a case whose author should hear about it.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::config::paths::{ensure_private_dir, set_private_file_mode};
use crate::domain::{
    AttachmentId, AttachmentInput, AttachmentKind, AttachmentMetadata, AttachmentSource,
    AttachmentState, CaseId,
};
use crate::error::{AppError, Result};
use crate::media::sniff::{sniff, Sniffed};

/// Why an attachment was refused at import.
///
/// The store returns this rather than a bare error because a refusal is a
/// durable fact about the attachment: it is recorded as `rejected` with the
/// reason attached, so a user asking why their video never reached the model
/// gets an answer instead of a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRejection {
    /// A stable, machine-readable reason code.
    pub code: RejectionCode,
    /// A human-readable explanation, safe to show a user.
    pub reason: String,
}

impl ImportRejection {
    fn new(code: RejectionCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ImportRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.reason)
    }
}

impl From<ImportRejection> for AppError {
    fn from(rejection: ImportRejection) -> Self {
        AppError::Invalid {
            message: rejection.to_string(),
        }
    }
}

/// The reason an attachment was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionCode {
    /// The declaration violated the wire contract before any bytes were read.
    Declaration,
    /// The bytes exceeded the configured attachment cap.
    TooLarge,
    /// The bytes could not be obtained or decoded.
    Unreadable,
    /// The measured media type contradicted the declared one.
    TypeMismatch,
    /// The measured size contradicted the declared one.
    SizeMismatch,
    /// The measured digest contradicted the declared one.
    DigestMismatch,
}

impl RejectionCode {
    /// The stable wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RejectionCode::Declaration => "declaration_invalid",
            RejectionCode::TooLarge => "too_large",
            RejectionCode::Unreadable => "unreadable",
            RejectionCode::TypeMismatch => "type_mismatch",
            RejectionCode::SizeMismatch => "size_mismatch",
            RejectionCode::DigestMismatch => "digest_mismatch",
        }
    }
}

/// An attachment that passed import and now lives in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedAttachment {
    /// The verified metadata, in state `imported`.
    pub metadata: AttachmentMetadata,
    /// Absolute path to the stored bytes.
    pub path: PathBuf,
}

/// The local store of attachment bytes, one directory per case.
#[derive(Debug, Clone)]
pub struct MediaStore {
    root: PathBuf,
    max_attachment_bytes: u64,
}

impl MediaStore {
    /// Open a store rooted at the data directory's `media/` directory.
    pub fn new(root: impl Into<PathBuf>, max_attachment_bytes: u64) -> Self {
        Self {
            root: root.into(),
            max_attachment_bytes,
        }
    }

    /// Open the store this installation's configuration describes.
    ///
    /// The cap comes from settings rather than the contract constant, so an
    /// operator can lower it for a machine with little disk without a code
    /// change; it can never be raised above the contract limit, which is what
    /// every other layer has already been told "bounded" means.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::new(
            config.data_dir.media_dir(),
            config
                .settings
                .limits
                .max_attachment_bytes
                .min(crate::domain::limits::MAX_ATTACHMENT_BYTES),
        )
    }

    /// The store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The largest attachment this store accepts.
    pub fn max_attachment_bytes(&self) -> u64 {
        self.max_attachment_bytes
    }

    /// The directory owning one case's attachment bytes.
    pub fn case_dir(&self, case_id: CaseId) -> PathBuf {
        self.root.join(case_id.to_string())
    }

    /// Import one submitted attachment into the case's directory.
    ///
    /// On success the bytes are on disk with owner-only permissions and the
    /// returned metadata describes what was measured, not what was claimed.
    pub fn import(
        &self,
        case_id: CaseId,
        input: &AttachmentInput,
    ) -> std::result::Result<ImportedAttachment, ImportRejection> {
        let report = {
            let mut report = crate::domain::ValidationReport::new();
            input.validate_into(&mut report, "attachment");
            report
        };
        if !report.is_valid() {
            return Err(ImportRejection::new(
                RejectionCode::Declaration,
                report.to_string(),
            ));
        }

        let bytes = self.materialize(input)?;
        let size_bytes = bytes.len() as u64;
        if size_bytes > self.max_attachment_bytes {
            return Err(ImportRejection::new(
                RejectionCode::TooLarge,
                format!(
                    "attachment is {size_bytes} bytes, over the {} byte limit",
                    self.max_attachment_bytes
                ),
            ));
        }
        if let Some(declared) = input.size_bytes {
            if declared != size_bytes {
                return Err(ImportRejection::new(
                    RejectionCode::SizeMismatch,
                    format!("declared {declared} bytes but the content is {size_bytes} bytes"),
                ));
            }
        }

        let digest = hex_digest(&bytes);
        if let Some(declared) = &input.digest_sha256 {
            if !declared.eq_ignore_ascii_case(&digest) {
                return Err(ImportRejection::new(
                    RejectionCode::DigestMismatch,
                    "the content does not match the declared SHA-256 digest",
                ));
            }
        }

        let media_type = measured_media_type(input, &bytes)?;

        let id = AttachmentId::new();
        let dir = self.case_dir(case_id);
        ensure_private_dir(&dir).map_err(|error| {
            ImportRejection::new(
                RejectionCode::Unreadable,
                format!("could not open the case media directory: {error}"),
            )
        })?;
        let path = dir.join(format!("{id}{}", extension_for(&media_type)));
        fs::write(&path, &bytes).map_err(|error| {
            ImportRejection::new(
                RejectionCode::Unreadable,
                format!("could not write the attachment: {error}"),
            )
        })?;
        set_private_file_mode(&path).map_err(|error| {
            ImportRejection::new(
                RejectionCode::Unreadable,
                format!("could not restrict the attachment file: {error}"),
            )
        })?;

        Ok(ImportedAttachment {
            metadata: AttachmentMetadata {
                id,
                name: input.name.clone(),
                kind: input.kind,
                media_type,
                size_bytes,
                digest_sha256: digest,
                state: AttachmentState::Imported,
                provider_file_name: None,
                uploaded_at: None,
                expires_at: None,
                rejected_reason: None,
            },
            path,
        })
    }

    /// Read stored bytes back, verifying they still match the recorded digest.
    ///
    /// The verification is not paranoia about disk corruption so much as about
    /// identity: these bytes are about to be sent to a third party under a
    /// digest Refinery published, and a case's own record is the only thing
    /// that says what was supposed to be sent.
    pub fn read_verified(&self, path: &Path, metadata: &AttachmentMetadata) -> Result<Vec<u8>> {
        let bytes = fs::read(path).map_err(|source| AppError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if hex_digest(&bytes) != metadata.digest_sha256 {
            return Err(AppError::Invalid {
                message: format!(
                    "stored bytes for attachment {} no longer match their recorded digest",
                    metadata.id
                ),
            });
        }
        Ok(bytes)
    }

    /// Delete a case's media directory and everything under it.
    pub fn remove_case(&self, case_id: CaseId) -> Result<()> {
        let dir = self.case_dir(case_id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(AppError::Io { path: dir, source }),
        }
    }

    /// Obtain the bytes named by an attachment's source.
    fn materialize(
        &self,
        input: &AttachmentInput,
    ) -> std::result::Result<Vec<u8>, ImportRejection> {
        match &input.source {
            AttachmentSource::InlineText { text } => Ok(text.as_bytes().to_vec()),
            AttachmentSource::InlineBase64 { data } => base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|error| {
                    ImportRejection::new(
                        RejectionCode::Unreadable,
                        format!("the inline content is not valid base64: {error}"),
                    )
                }),
            AttachmentSource::LocalPath { path } => self.read_local(Path::new(path)),
        }
    }

    /// Read a submitter-supplied local file, refusing an oversized one before
    /// it is read into memory rather than after.
    fn read_local(&self, path: &Path) -> std::result::Result<Vec<u8>, ImportRejection> {
        let metadata = fs::metadata(path).map_err(|error| {
            ImportRejection::new(
                RejectionCode::Unreadable,
                format!("could not read the attachment file: {error}"),
            )
        })?;
        if !metadata.is_file() {
            return Err(ImportRejection::new(
                RejectionCode::Unreadable,
                "the attachment path is not a regular file",
            ));
        }
        if metadata.len() > self.max_attachment_bytes {
            return Err(ImportRejection::new(
                RejectionCode::TooLarge,
                format!(
                    "attachment is {} bytes, over the {} byte limit",
                    metadata.len(),
                    self.max_attachment_bytes
                ),
            ));
        }
        // Read one byte past the cap so a file that grew between the metadata
        // call and the read is caught by the size check rather than accepted.
        let file = fs::File::open(path).map_err(|error| {
            ImportRejection::new(
                RejectionCode::Unreadable,
                format!("could not open the attachment file: {error}"),
            )
        })?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(self.max_attachment_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| {
                ImportRejection::new(
                    RejectionCode::Unreadable,
                    format!("could not read the attachment file: {error}"),
                )
            })?;
        Ok(bytes)
    }
}

/// Decide the media type to record, refusing a declaration the bytes contradict.
///
/// Anything Refinery will present to a provider as an image, a video, or text
/// must be positively measured. For a plain `file` attachment an unrecognised
/// signature is normal — Refinery has no opinion about a `.tar.zst` — so the
/// declaration stands, but it can never turn unrecognised bytes into a video.
fn measured_media_type(
    input: &AttachmentInput,
    bytes: &[u8],
) -> std::result::Result<String, ImportRejection> {
    let declared = normalize_media_type(&input.media_type);
    match sniff(bytes) {
        Sniffed::Known(measured) => {
            if declared != measured || !kind_matches(input.kind, measured) {
                return Err(ImportRejection::new(
                    RejectionCode::TypeMismatch,
                    format!(
                        "declared {} {} but the content is {measured}",
                        input.kind.as_str(),
                        declared
                    ),
                ));
            }
            Ok(measured.to_owned())
        }
        Sniffed::Text => {
            if input.kind != AttachmentKind::Text && input.kind != AttachmentKind::File {
                return Err(ImportRejection::new(
                    RejectionCode::TypeMismatch,
                    format!("declared {} but the content is text", input.kind.as_str()),
                ));
            }
            if !is_textual(&declared) {
                return Err(ImportRejection::new(
                    RejectionCode::TypeMismatch,
                    format!("declared {declared} but the content is text"),
                ));
            }
            Ok(declared)
        }
        Sniffed::Unknown => {
            if input.kind != AttachmentKind::File {
                return Err(ImportRejection::new(
                    RejectionCode::TypeMismatch,
                    format!(
                        "declared {} {declared} but the content matches no recognised {} format",
                        input.kind.as_str(),
                        input.kind.as_str()
                    ),
                ));
            }
            Ok(declared)
        }
    }
}

/// Whether a measured type belongs to the category the submitter declared.
fn kind_matches(kind: AttachmentKind, media_type: &str) -> bool {
    match kind {
        AttachmentKind::Image => media_type.starts_with("image/"),
        AttachmentKind::Video => media_type.starts_with("video/"),
        AttachmentKind::Text => media_type.starts_with("text/"),
        AttachmentKind::File => true,
    }
}

/// Whether a media type describes content that is legitimately UTF-8 text.
fn is_textual(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/javascript"
                | "application/sql"
        )
}

/// Lowercase a media type and drop any parameters such as `; charset=utf-8`,
/// so `Image/PNG` and `image/png` are one type rather than two.
fn normalize_media_type(declared: &str) -> String {
    let base = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "image/jpg" => "image/jpeg".to_owned(),
        "video/mpeg4" => "video/mp4".to_owned(),
        _ => base,
    }
}

/// The on-disk extension for a stored attachment. It is cosmetic — nothing
/// reads it back — but a media directory a user can open and understand is
/// worth the few lines.
fn extension_for(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        "application/pdf" => ".pdf",
        "video/mp4" => ".mp4",
        "video/quicktime" => ".mov",
        "video/webm" => ".webm",
        "video/x-matroska" => ".mkv",
        "text/plain" => ".txt",
        "text/markdown" => ".md",
        "application/json" => ".json",
        _ => ".bin",
    }
}

/// The lowercase hex SHA-256 of a byte slice.
pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";

    fn store(dir: &TempDir) -> MediaStore {
        MediaStore::new(dir.path().join("media"), 1024)
    }

    fn png_input(source: AttachmentSource) -> AttachmentInput {
        AttachmentInput {
            name: "shot.png".into(),
            kind: AttachmentKind::Image,
            media_type: "image/png".into(),
            size_bytes: None,
            digest_sha256: None,
            source,
        }
    }

    fn inline_png() -> AttachmentSource {
        AttachmentSource::InlineBase64 {
            data: base64::engine::general_purpose::STANDARD.encode(PNG),
        }
    }

    #[test]
    fn an_imported_image_is_measured_stored_and_owner_only() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let case = CaseId::new();
        let imported = store.import(case, &png_input(inline_png())).unwrap();

        assert_eq!(imported.metadata.state, AttachmentState::Imported);
        assert_eq!(imported.metadata.media_type, "image/png");
        assert_eq!(imported.metadata.size_bytes, PNG.len() as u64);
        assert_eq!(imported.metadata.digest_sha256, hex_digest(PNG));
        assert_eq!(fs::read(&imported.path).unwrap(), PNG);
        assert!(imported.path.starts_with(store.case_dir(case)));
        assert_eq!(imported.path.extension().unwrap(), "png");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&imported.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_declared_type_the_bytes_contradict_is_rejected() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let mut input = png_input(AttachmentSource::InlineBase64 {
            data: base64::engine::general_purpose::STANDARD.encode([0xFF, 0xD8, 0xFF, 0xE0]),
        });
        input.media_type = "image/png".into();
        let rejection = store.import(CaseId::new(), &input).unwrap_err();
        assert_eq!(rejection.code, RejectionCode::TypeMismatch);
    }

    #[test]
    fn text_bytes_may_not_be_passed_off_as_video() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let input = AttachmentInput {
            name: "clip.mp4".into(),
            kind: AttachmentKind::Video,
            media_type: "video/mp4".into(),
            size_bytes: None,
            digest_sha256: None,
            source: AttachmentSource::InlineText {
                text: "not a video".into(),
            },
        };
        let rejection = store.import(CaseId::new(), &input).unwrap_err();
        assert_eq!(rejection.code, RejectionCode::TypeMismatch);
    }

    #[test]
    fn unrecognised_bytes_are_accepted_only_as_a_plain_file() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let opaque = base64::engine::general_purpose::STANDARD.encode([0x00, 0x01, 0x02, 0x03]);

        let mut as_image = png_input(AttachmentSource::InlineBase64 {
            data: opaque.clone(),
        });
        as_image.media_type = "image/png".into();
        assert_eq!(
            store.import(CaseId::new(), &as_image).unwrap_err().code,
            RejectionCode::TypeMismatch
        );

        let as_file = AttachmentInput {
            name: "archive.bin".into(),
            kind: AttachmentKind::File,
            media_type: "application/octet-stream".into(),
            size_bytes: None,
            digest_sha256: None,
            source: AttachmentSource::InlineBase64 { data: opaque },
        };
        let imported = store.import(CaseId::new(), &as_file).unwrap();
        assert_eq!(imported.metadata.media_type, "application/octet-stream");
    }

    #[test]
    fn an_oversized_attachment_is_rejected_inline_and_from_disk() {
        let dir = TempDir::new().unwrap();
        let store = MediaStore::new(dir.path().join("media"), 8);
        let big = vec![b'a'; 64];

        let inline = AttachmentInput {
            name: "big.txt".into(),
            kind: AttachmentKind::Text,
            media_type: "text/plain".into(),
            size_bytes: None,
            digest_sha256: None,
            source: AttachmentSource::InlineText {
                text: String::from_utf8(big.clone()).unwrap(),
            },
        };
        assert_eq!(
            store.import(CaseId::new(), &inline).unwrap_err().code,
            RejectionCode::TooLarge
        );

        let file = dir.path().join("big.txt");
        fs::write(&file, &big).unwrap();
        let local = AttachmentInput {
            source: AttachmentSource::LocalPath {
                path: file.to_string_lossy().into_owned(),
            },
            ..inline
        };
        assert_eq!(
            store.import(CaseId::new(), &local).unwrap_err().code,
            RejectionCode::TooLarge
        );
    }

    #[test]
    fn a_declared_digest_or_size_that_does_not_hold_is_rejected() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);

        let mut wrong_digest = png_input(inline_png());
        wrong_digest.digest_sha256 = Some("b".repeat(64));
        assert_eq!(
            store.import(CaseId::new(), &wrong_digest).unwrap_err().code,
            RejectionCode::DigestMismatch
        );

        let mut wrong_size = png_input(inline_png());
        wrong_size.size_bytes = Some(PNG.len() as u64 + 1);
        assert_eq!(
            store.import(CaseId::new(), &wrong_size).unwrap_err().code,
            RejectionCode::SizeMismatch
        );

        let mut right = png_input(inline_png());
        right.digest_sha256 = Some(hex_digest(PNG));
        right.size_bytes = Some(PNG.len() as u64);
        assert!(store.import(CaseId::new(), &right).is_ok());
    }

    #[test]
    fn an_unreadable_source_is_rejected_rather_than_panicking() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let mut input = png_input(AttachmentSource::InlineBase64 {
            data: "not base64!!".into(),
        });
        input.media_type = "image/png".into();
        assert_eq!(
            store.import(CaseId::new(), &input).unwrap_err().code,
            RejectionCode::Unreadable
        );

        let missing = png_input(AttachmentSource::LocalPath {
            path: dir.path().join("absent.png").to_string_lossy().into_owned(),
        });
        assert_eq!(
            store.import(CaseId::new(), &missing).unwrap_err().code,
            RejectionCode::Unreadable
        );
    }

    #[test]
    fn a_local_file_is_copied_once_and_survives_its_original() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let source = dir.path().join("shot.png");
        fs::write(&source, PNG).unwrap();
        let imported = store
            .import(
                CaseId::new(),
                &png_input(AttachmentSource::LocalPath {
                    path: source.to_string_lossy().into_owned(),
                }),
            )
            .unwrap();
        fs::remove_file(&source).unwrap();
        assert_eq!(
            store
                .read_verified(&imported.path, &imported.metadata)
                .unwrap(),
            PNG
        );
    }

    #[test]
    fn reading_back_bytes_that_changed_underneath_the_store_fails() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let imported = store
            .import(CaseId::new(), &png_input(inline_png()))
            .unwrap();
        fs::write(&imported.path, b"\x89PNG\r\n\x1a\ntampered").unwrap();
        assert!(store
            .read_verified(&imported.path, &imported.metadata)
            .is_err());
    }

    #[test]
    fn removing_a_case_clears_its_media_and_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir);
        let case = CaseId::new();
        store.import(case, &png_input(inline_png())).unwrap();
        assert!(store.case_dir(case).is_dir());
        store.remove_case(case).unwrap();
        assert!(!store.case_dir(case).exists());
        store.remove_case(case).unwrap();
    }

    #[test]
    fn media_types_are_normalized_before_comparison() {
        assert_eq!(normalize_media_type("Image/JPG"), "image/jpeg");
        assert_eq!(
            normalize_media_type("text/plain; charset=utf-8"),
            "text/plain"
        );
    }
}
