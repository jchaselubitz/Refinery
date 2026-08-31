//! The media store and the Gemini upload lifecycle, end to end.
//!
//! These tests run the real import path, the real storage intents, and the
//! real Files API client against a fake provider that serves recorded response
//! bodies from `tests/fixtures/gemini`. Nothing here reaches the network or
//! reads a credential, and the fake counts what it was asked to do, so a test
//! can assert that a re-upload actually happened rather than inferring it from
//! a state column.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};

use refinery::domain::{
    AttachmentInput, AttachmentKind, AttachmentSource, AttachmentState, CaseEventPayload, CaseId,
    SecretString,
};
use refinery::media::{
    ensure_available, import_request_attachments, ActivationPolicy, GeminiFilesClient, MediaStore,
};
use refinery::storage::Storage;

const UPLOAD_IMAGE_ACTIVE: &str = include_str!("../fixtures/gemini/upload_image_active.json");
const UPLOAD_VIDEO_PROCESSING: &str =
    include_str!("../fixtures/gemini/upload_video_processing.json");
const GET_VIDEO_PROCESSING: &str = include_str!("../fixtures/gemini/get_video_processing.json");
const GET_VIDEO_ACTIVE: &str = include_str!("../fixtures/gemini/get_video_active.json");
const ERROR_UNAVAILABLE: &str = include_str!("../fixtures/gemini/error_unavailable.json");

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";

fn mp4() -> Vec<u8> {
    [
        b"\x00\x00\x00\x18".as_slice(),
        b"ftyp",
        b"isom",
        b"\x00\x00\x02\x00mp41",
    ]
    .concat()
}

/// What the fake provider should pretend to be holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Media {
    Image,
    Video,
}

/// A fake Gemini Files API that serves recorded bodies and counts requests.
struct FakeProvider {
    media: Media,
    /// How many finalize requests to reject before accepting one, so a test
    /// can drive the failure path without a second server.
    fail_uploads: AtomicUsize,
    uploads: AtomicUsize,
    gets: AtomicUsize,
    /// The expiry stamped into every served body. Controlling it is what lets
    /// a test observe expiry without waiting two days.
    expires_at: DateTime<Utc>,
}

impl FakeProvider {
    fn new(media: Media, expires_at: DateTime<Utc>) -> Arc<Self> {
        Arc::new(Self {
            media,
            fail_uploads: AtomicUsize::new(0),
            uploads: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
            expires_at,
        })
    }

    fn render(&self, body: &str, name: &str) -> String {
        body.replace("__NAME__", name)
            .replace("__EXPIRES__", &self.expires_at.to_rfc3339())
    }
}

/// A running fake provider and the base URL that addresses it.
struct RunningProvider {
    base_url: String,
    state: Arc<FakeProvider>,
    _task: tokio::task::JoinHandle<()>,
}

async fn start_provider(state: Arc<FakeProvider>) -> RunningProvider {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind fake provider");
    let addr = listener.local_addr().expect("provider address");
    let base_url = format!("http://{addr}/v1beta");
    let app = Router::new()
        .route("/v1beta/files", post(start_upload))
        .route("/upload/{token}", post(finalize_upload))
        .route("/v1beta/files/{name}", get(get_file))
        .with_state((state.clone(), format!("http://{addr}")));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    RunningProvider {
        base_url,
        state,
        _task: task,
    }
}

type ProviderState = State<(Arc<FakeProvider>, String)>;

/// Step one of the resumable protocol: hand back a one-shot upload URL.
async fn start_upload(
    State((_state, origin)): ProviderState,
    headers: HeaderMap,
) -> impl IntoResponse {
    assert_eq!(
        headers
            .get("x-goog-upload-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("resumable"),
        "the client must use the resumable upload protocol"
    );
    assert!(
        headers.contains_key("x-goog-upload-header-content-length"),
        "the client must declare the content length up front"
    );
    assert!(
        headers.contains_key("x-goog-api-key"),
        "every request must carry the API key header"
    );
    let mut response = HeaderMap::new();
    response.insert(
        "x-goog-upload-url",
        format!("{origin}/upload/one").parse().unwrap(),
    );
    (StatusCode::OK, response, String::new())
}

/// Step two: accept the bytes and describe the resulting file.
async fn finalize_upload(
    State((state, _origin)): ProviderState,
    AxumPath(_token): AxumPath<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    assert_eq!(
        headers
            .get("x-goog-upload-command")
            .and_then(|value| value.to_str().ok()),
        Some("upload, finalize")
    );
    assert!(
        !body.is_empty(),
        "the finalize request must carry the bytes"
    );

    if state.fail_uploads.load(Ordering::SeqCst) > 0 {
        state.fail_uploads.fetch_sub(1, Ordering::SeqCst);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            ERROR_UNAVAILABLE.to_owned(),
        );
    }

    let attempt = state.uploads.fetch_add(1, Ordering::SeqCst) + 1;
    let name = format!("files/upload-{attempt}");
    let body = match state.media {
        Media::Image => state.render(UPLOAD_IMAGE_ACTIVE, &name),
        Media::Video => state.render(UPLOAD_VIDEO_PROCESSING, &name),
    };
    (StatusCode::OK, body)
}

/// Activation polling: video reports processing once, then active.
async fn get_file(
    State((state, _origin)): ProviderState,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let polls = state.gets.fetch_add(1, Ordering::SeqCst);
    let full = format!("files/{name}");
    let body = if polls == 0 {
        state.render(GET_VIDEO_PROCESSING, &full)
    } else {
        state.render(GET_VIDEO_ACTIVE, &full)
    };
    (StatusCode::OK, body)
}

/// A storage, media store, and case ready to attach media to.
struct Harness {
    _temp: tempfile::TempDir,
    storage: Storage,
    store: MediaStore,
    case_id: CaseId,
}

async fn harness() -> Harness {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = Storage::open(temp.path().join("refinery.db"))
        .await
        .expect("open storage");
    let request: refinery::domain::RefinementRequest = serde_json::from_str(include_str!(
        "../fixtures/contracts/refinement_request.json"
    ))
    .expect("parse request fixture");
    let case_id = storage
        .create_case_idempotent(&request)
        .await
        .expect("create case")
        .case_id();
    let store = MediaStore::new(temp.path().join("media"), 1024 * 1024);
    Harness {
        _temp: temp,
        storage,
        store,
        case_id,
    }
}

fn inline(name: &str, kind: AttachmentKind, media_type: &str, bytes: &[u8]) -> AttachmentInput {
    AttachmentInput {
        name: name.to_owned(),
        kind,
        media_type: media_type.to_owned(),
        size_bytes: Some(bytes.len() as u64),
        digest_sha256: Some(refinery::media::hex_digest(bytes)),
        source: AttachmentSource::InlineBase64 {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    }
}

fn client(provider: &RunningProvider) -> GeminiFilesClient {
    GeminiFilesClient::new(&provider.base_url, SecretString::new("test-key"))
        .expect("build files client")
        .with_activation_policy(ActivationPolicy {
            interval: std::time::Duration::from_millis(5),
            max_wait: std::time::Duration::from_secs(5),
        })
}

#[tokio::test]
async fn import_records_verified_attachments_and_refuses_ones_the_bytes_contradict() {
    let harness = harness().await;

    let good = inline("screenshot.png", AttachmentKind::Image, "image/png", PNG);
    let mut wrong_type = inline("clip.mp4", AttachmentKind::Video, "video/mp4", PNG);
    wrong_type.name = "clip.mp4".into();
    let mut wrong_size = inline("notes.txt", AttachmentKind::Text, "text/plain", b"hello");
    wrong_size.size_bytes = Some(9_999);
    let mut wrong_digest = inline("second.png", AttachmentKind::Image, "image/png", PNG);
    wrong_digest.digest_sha256 = Some("b".repeat(64));

    let summary = import_request_attachments(
        &harness.storage,
        &harness.store,
        harness.case_id,
        &[good, wrong_type, wrong_size, wrong_digest],
    )
    .await
    .expect("import");

    assert_eq!(summary.imported.len(), 1);
    assert_eq!(summary.rejected.len(), 3);
    assert!(!summary.is_clean());

    let imported = &summary.imported[0];
    assert_eq!(imported.state, AttachmentState::Imported);
    assert_eq!(imported.media_type, "image/png");
    assert_eq!(imported.size_bytes, PNG.len() as u64);
    assert_eq!(imported.digest_sha256, refinery::media::hex_digest(PNG));

    for rejected in &summary.rejected {
        assert_eq!(rejected.state, AttachmentState::Rejected);
        let reason = rejected.rejected_reason.as_deref().expect("a reason");
        assert!(!reason.is_empty(), "a refusal must say why");
    }
    let reasons: Vec<&str> = summary
        .rejected
        .iter()
        .map(|attachment| attachment.rejected_reason.as_deref().unwrap())
        .collect();
    assert!(reasons
        .iter()
        .any(|reason| reason.contains("type_mismatch")));
    assert!(reasons
        .iter()
        .any(|reason| reason.contains("size_mismatch")));
    assert!(reasons
        .iter()
        .any(|reason| reason.contains("digest_mismatch")));

    // Every attachment, accepted or not, is durable and on the case history.
    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read attachments");
    assert_eq!(stored.len(), 4);
    assert_eq!(
        stored
            .iter()
            .filter(|attachment| attachment.media_path.is_some())
            .count(),
        1,
        "only the accepted attachment has local bytes"
    );

    let events = harness
        .storage
        .case_events(harness.case_id)
        .await
        .expect("events");
    let imported_events = events
        .iter()
        .filter(|event| matches!(event.payload, CaseEventPayload::AttachmentImported { .. }))
        .count();
    let rejected_events = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                CaseEventPayload::AttachmentStateChanged {
                    state: AttachmentState::Rejected,
                    ..
                }
            )
        })
        .count();
    assert_eq!(imported_events, 1);
    assert_eq!(rejected_events, 3);
}

#[tokio::test]
async fn an_image_uploads_once_and_records_the_provider_name_and_expiry() {
    let harness = harness().await;
    let expires_at = Utc::now() + Duration::hours(47);
    let provider = start_provider(FakeProvider::new(Media::Image, expires_at)).await;
    let files = client(&provider);

    import_request_attachments(
        &harness.storage,
        &harness.store,
        harness.case_id,
        &[inline(
            "screenshot.png",
            AttachmentKind::Image,
            "image/png",
            PNG,
        )],
    )
    .await
    .expect("import");

    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read")
        .remove(0);
    let now = Utc::now();
    let available = ensure_available(&harness.storage, &harness.store, &files, &stored, now)
        .await
        .expect("upload");

    assert_eq!(available.state, AttachmentState::Available);
    assert_eq!(
        available.provider_file_name.as_deref(),
        Some("files/upload-1")
    );
    assert_eq!(available.expires_at, Some(expires_at));
    assert_eq!(available.uploaded_at, Some(now));
    assert_eq!(provider.state.uploads.load(Ordering::SeqCst), 1);

    // A second use inside the expiry window must not re-upload.
    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read")
        .remove(0);
    let again = ensure_available(&harness.storage, &harness.store, &files, &stored, now)
        .await
        .expect("reuse");
    assert_eq!(again.provider_file_name, available.provider_file_name);
    assert_eq!(provider.state.uploads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_video_is_polled_to_activation_before_it_is_reported_available() {
    let harness = harness().await;
    let provider = start_provider(FakeProvider::new(
        Media::Video,
        Utc::now() + Duration::hours(47),
    ))
    .await;
    let files = client(&provider);

    let bytes = mp4();
    import_request_attachments(
        &harness.storage,
        &harness.store,
        harness.case_id,
        &[inline(
            "capture.mp4",
            AttachmentKind::Video,
            "video/mp4",
            &bytes,
        )],
    )
    .await
    .expect("import");

    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read")
        .remove(0);
    let available = ensure_available(
        &harness.storage,
        &harness.store,
        &files,
        &stored,
        Utc::now(),
    )
    .await
    .expect("upload video");

    assert_eq!(available.state, AttachmentState::Available);
    assert_eq!(available.media_type, "video/mp4");
    assert!(
        provider.state.gets.load(Ordering::SeqCst) >= 2,
        "a processing video must be polled until it activates"
    );
}

#[tokio::test]
async fn an_expired_provider_copy_is_re_uploaded_from_the_local_bytes() {
    let harness = harness().await;
    // The provider hands back a copy that expires in an hour, so a case
    // resumed two hours later — an overnight wait for an answer, compressed —
    // finds it gone.
    let expires_at = Utc::now() + Duration::hours(1);
    let provider = start_provider(FakeProvider::new(Media::Image, expires_at)).await;
    let files = client(&provider);

    import_request_attachments(
        &harness.storage,
        &harness.store,
        harness.case_id,
        &[inline(
            "screenshot.png",
            AttachmentKind::Image,
            "image/png",
            PNG,
        )],
    )
    .await
    .expect("import");

    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read")
        .remove(0);
    let first = ensure_available(
        &harness.storage,
        &harness.store,
        &files,
        &stored,
        Utc::now(),
    )
    .await
    .expect("first upload");
    assert_eq!(first.provider_file_name.as_deref(), Some("files/upload-1"));

    let later = Utc::now() + Duration::hours(2);
    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read")
        .remove(0);
    let second = ensure_available(&harness.storage, &harness.store, &files, &stored, later)
        .await
        .expect("re-upload after expiry");

    assert_eq!(second.state, AttachmentState::Available);
    assert_eq!(second.provider_file_name.as_deref(), Some("files/upload-2"));
    assert_eq!(
        provider.state.uploads.load(Ordering::SeqCst),
        2,
        "expiry must trigger a second upload of the same local bytes"
    );

    // The history must explain the second upload rather than leave a reader
    // wondering why the same attachment was sent twice.
    let states: Vec<AttachmentState> = harness
        .storage
        .case_events(harness.case_id)
        .await
        .expect("events")
        .into_iter()
        .filter_map(|event| match event.payload {
            CaseEventPayload::AttachmentStateChanged { state, .. } => Some(state),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![
            AttachmentState::Uploading,
            AttachmentState::Available,
            AttachmentState::Expired,
            AttachmentState::Uploading,
            AttachmentState::Available,
        ]
    );
}

#[tokio::test]
async fn a_failed_upload_leaves_the_attachment_ready_to_try_again() {
    let harness = harness().await;
    let state = FakeProvider::new(Media::Image, Utc::now() + Duration::hours(47));
    state.fail_uploads.store(1, Ordering::SeqCst);
    let provider = start_provider(state).await;
    let files = client(&provider);

    import_request_attachments(
        &harness.storage,
        &harness.store,
        harness.case_id,
        &[inline(
            "screenshot.png",
            AttachmentKind::Image,
            "image/png",
            PNG,
        )],
    )
    .await
    .expect("import");

    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read")
        .remove(0);
    let error = ensure_available(
        &harness.storage,
        &harness.store,
        &files,
        &stored,
        Utc::now(),
    )
    .await
    .expect_err("the provider refused this attempt");
    assert!(
        error.retry_class().is_retryable(),
        "a 503 from the provider must be retryable, got {error}"
    );

    let after = harness
        .storage
        .attachment(stored.metadata.id)
        .await
        .expect("read back");
    assert_eq!(after.metadata.state, AttachmentState::Imported);
    assert_eq!(after.metadata.provider_file_name, None);
    assert!(
        after.media_path.is_some(),
        "the local bytes must survive a failed upload"
    );

    // The retry succeeds against the same bytes with no new import.
    let retried = ensure_available(&harness.storage, &harness.store, &files, &after, Utc::now())
        .await
        .expect("retry");
    assert_eq!(retried.state, AttachmentState::Available);
}

#[tokio::test]
async fn a_rejected_attachment_is_never_uploaded() {
    let harness = harness().await;
    let provider = start_provider(FakeProvider::new(
        Media::Image,
        Utc::now() + Duration::hours(47),
    ))
    .await;
    let files = client(&provider);

    let mut bad = inline("clip.mp4", AttachmentKind::Video, "video/mp4", PNG);
    bad.digest_sha256 = None;
    import_request_attachments(&harness.storage, &harness.store, harness.case_id, &[bad])
        .await
        .expect("import");

    let stored = harness
        .storage
        .case_attachments(harness.case_id)
        .await
        .expect("read")
        .remove(0);
    assert_eq!(stored.metadata.state, AttachmentState::Rejected);
    assert!(ensure_available(
        &harness.storage,
        &harness.store,
        &files,
        &stored,
        Utc::now()
    )
    .await
    .is_err());
    assert_eq!(provider.state.uploads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn preparing_a_case_makes_every_usable_attachment_available_and_skips_the_rest() {
    let harness = harness().await;
    let provider = start_provider(FakeProvider::new(
        Media::Image,
        Utc::now() + Duration::hours(47),
    ))
    .await;
    let files = client(&provider);

    let mut rejected = inline("clip.mp4", AttachmentKind::Video, "video/mp4", PNG);
    rejected.digest_sha256 = None;
    import_request_attachments(
        &harness.storage,
        &harness.store,
        harness.case_id,
        &[
            inline("first.png", AttachmentKind::Image, "image/png", PNG),
            rejected,
            inline(
                "second.png",
                AttachmentKind::Image,
                "image/png",
                b"\x89PNG\r\n\x1a\nsecond",
            ),
        ],
    )
    .await
    .expect("import");

    let available = refinery::media::ensure_case_attachments_available(
        &harness.storage,
        &harness.store,
        &files,
        harness.case_id,
        Utc::now(),
    )
    .await
    .expect("prepare attachments");

    assert_eq!(available.len(), 2, "the rejected attachment is skipped");
    assert!(available
        .iter()
        .all(|attachment| attachment.state == AttachmentState::Available));
    assert_eq!(provider.state.uploads.load(Ordering::SeqCst), 2);
}
