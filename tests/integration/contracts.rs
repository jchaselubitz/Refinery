//! The contract freeze.
//!
//! Milestone M1 froze Refinery's public contracts. These tests are what makes
//! that real: each committed fixture is a document a client may legitimately
//! send or receive, and it must keep parsing, keep round-tripping byte-for-byte
//! through the current types, and keep validating. Renaming a field, changing
//! an enum tag, or tightening a rule breaks one of these, which is the signal
//! to bump the contract version rather than to edit the fixture.
//!
//! They complement the schema-drift check: drift proves the published schemas
//! describe the types, and these prove the types still accept the documents
//! those schemas promised.

use std::path::PathBuf;

use refinery::cases::{transition, TransitionError};
use refinery::domain::{
    AnswerSet, ApiError, BackendCapabilities, CaseEvent, CaseEventPayload, CaseState,
    CaseTransition, DeliveryEnvelope, DeliveryReceipt, PromptValidationContext, QuestionRequest,
    RefinedPrompt, RefinementRequest, SchemaVersion, CONTRACT_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/contracts")
        .join(name)
}

fn read_fixture(name: &str) -> serde_json::Value {
    let path = fixture_path(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()))
}

/// Parse a fixture and assert it survives a round trip unchanged.
///
/// Equality is on the JSON value, not the text, so formatting is free but
/// every field name, enum tag, and value shape is pinned.
fn round_trip<T>(name: &str) -> T
where
    T: Serialize + DeserializeOwned,
{
    let original = read_fixture(name);
    let parsed: T = serde_json::from_value(original.clone())
        .unwrap_or_else(|error| panic!("{name} no longer parses: {error}"));
    let reserialized = serde_json::to_value(&parsed)
        .unwrap_or_else(|error| panic!("{name} no longer serializes: {error}"));
    assert_eq!(
        reserialized, original,
        "{name} did not round-trip; a field name or shape changed"
    );
    parsed
}

#[test]
fn a_refinement_request_round_trips_and_validates() {
    let request: RefinementRequest = round_trip("refinement_request.json");
    request
        .validate()
        .expect("the frozen request must validate");
    assert_eq!(request.schema_version, SchemaVersion::CURRENT);
    assert!(request.needs_image_input());
    assert!(!request.needs_video_input());
}

#[test]
fn a_question_request_and_its_answers_round_trip_and_agree() {
    let request: QuestionRequest = round_trip("question_request.json");
    request
        .validate()
        .expect("the frozen request must validate");

    let answers: AnswerSet = round_trip("answer_set.json");
    request
        .validate_answers(&answers)
        .expect("the frozen answers must satisfy the frozen questions");
    assert!(request.unresolved_required(&answers).is_empty());
}

#[test]
fn a_refined_prompt_round_trips_and_passes_every_deterministic_check() {
    let prompt: RefinedPrompt = round_trip("refined_prompt.json");
    let context = PromptValidationContext {
        unresolved_required_questions: vec![],
        known_attachments: vec!["018f6b3a-0000-7000-8000-000000000020".parse().unwrap()],
    };
    prompt
        .validate(&context)
        .expect("the frozen prompt must validate");
}

#[test]
fn a_case_event_round_trips_with_its_transition_intact() {
    let event: CaseEvent = round_trip("case_event.json");
    assert_eq!(event.sequence, 7);
    let CaseEventPayload::StateChanged {
        from,
        to,
        transition: edge,
    } = &event.payload
    else {
        panic!("the frozen event must be a state change");
    };
    assert_eq!(*from, CaseState::Running);
    assert_eq!(*to, CaseState::AwaitingAnswer);
    // The recorded edge must still be the one the state machine would take.
    assert_eq!(transition(*from, edge), Ok(*to));
    assert!(matches!(edge, CaseTransition::QuestionRaised { .. }));
}

#[test]
fn backend_capabilities_round_trip() {
    let capabilities: BackendCapabilities = round_trip("backend_capabilities.json");
    assert_eq!(capabilities.backend_id, "gemini");
    assert!(capabilities.native_video_input);
    assert!(!capabilities.repository_runtime);
}

#[test]
fn a_delivery_envelope_and_receipt_round_trip() {
    let envelope: DeliveryEnvelope = round_trip("delivery_envelope.json");
    assert_eq!(
        envelope.idempotency_key,
        DeliveryEnvelope::attempt_key(envelope.delivery_id, 1),
        "the frozen key must still be what the current derivation produces"
    );
    let receipt: DeliveryReceipt = round_trip("delivery_receipt.json");
    assert!(receipt.accepted);
}

#[test]
fn the_uniform_error_body_round_trips() {
    let error: ApiError = round_trip("api_error.json");
    assert_eq!(error.code, "invalid_request");
    assert_eq!(error.issues.len(), 1);
}

#[test]
fn every_case_state_has_a_stable_wire_spelling() {
    // The published `case_state.json` schema enumerates these, so a rename is a
    // contract change even though no fixture document contains only a state.
    let expected = [
        "received",
        "preparing",
        "running",
        "awaiting_answer",
        "validating",
        "ready",
        "delivering",
        "completed",
        "failed",
        "cancelled",
    ];
    let actual: Vec<String> = CaseState::ALL
        .iter()
        .map(|state| {
            serde_json::to_value(state)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn the_committed_schemas_describe_this_contract_version() {
    let schemas = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(schemas.join("contract.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["contract_version"], CONTRACT_VERSION);

    for name in [
        "refinement_request.json",
        "question_request.json",
        "answer_set.json",
        "refined_prompt.json",
        "case_state.json",
        "case_event.json",
        "backend_capabilities.json",
        "delivery_envelope.json",
        "delivery_receipt.json",
        "api_error.json",
    ] {
        let path = schemas.join(name);
        assert!(path.is_file(), "{name} must be committed under schemas/");
        let schema: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            schema.get("$schema").is_some(),
            "{name} must be a JSON Schema document"
        );
    }
}

#[test]
fn a_document_from_a_future_contract_version_is_refused_rather_than_misread() {
    let mut value = read_fixture("refinement_request.json");
    value["schema_version"] = serde_json::json!(CONTRACT_VERSION + 1);
    let request: RefinementRequest = serde_json::from_value(value).unwrap();
    let report = request
        .validate()
        .expect_err("a future contract version must be refused");
    assert!(report.to_string().contains("schema version"));
}

#[test]
fn an_illegal_transition_names_what_was_attempted() {
    let error = transition(
        CaseState::Completed,
        &CaseTransition::Cancelled { reason: None },
    )
    .expect_err("a completed case cannot be cancelled");
    let TransitionError::Illegal { from, .. } = &error;
    assert_eq!(*from, CaseState::Completed);
}
