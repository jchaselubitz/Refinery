//! Generate the committed JSON Schemas for Refinery's public contracts.
//!
//! `cargo run --bin refinery-schemas -- --out schemas` writes one file per
//! contract. `scripts/check-schema-drift.sh` runs the same generator into a
//! temporary directory and diffs it against `schemas/`, so a contract change
//! that is not accompanied by regenerated schemas fails CI.
//!
//! The generated files are the integration surface for Overlord and any future
//! client: they are what lets a caller validate a submission without importing
//! Rust. Adding a contract here is therefore a deliberate act — it publishes
//! the type.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use schemars::schema_for;

use refinery::domain::{
    AnswerSet, ApiError, BackendCapabilities, CaseEvent, CaseState, DeliveryEnvelope,
    DeliveryReceipt, QuestionRequest, RefinedPrompt, RefinementRequest, CONTRACT_VERSION,
};

/// One published contract: the file it lives in and its schema.
macro_rules! contracts {
    ($($file:literal => $type:ty),+ $(,)?) => {
        vec![$(($file, serde_json::to_value(schema_for!($type)).expect("schema is serializable"))),+]
    };
}

fn main() -> ExitCode {
    let out_dir = match parse_out_dir() {
        Ok(dir) => dir,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: refinery-schemas --out <directory>");
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = write_schemas(&out_dir) {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn parse_out_dir() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut out_dir = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => {
                out_dir = Some(PathBuf::from(args.next().ok_or("--out needs a directory")?));
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    out_dir.ok_or_else(|| "--out is required".to_owned())
}

fn write_schemas(out_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("creating {}: {e}", out_dir.display()))?;

    // The published contracts. Wire roots only: a type that appears solely
    // inside another contract is already described by that contract's
    // definitions, and publishing it separately would give clients two places
    // to look for the same rules.
    let schemas = contracts! {
        // Ingress: what a source submits.
        "refinement_request.json" => RefinementRequest,
        // Interaction: what Refinery asks and what comes back.
        "question_request.json" => QuestionRequest,
        "answer_set.json" => AnswerSet,
        // Output: what the agent must produce.
        "refined_prompt.json" => RefinedPrompt,
        // Observation: case state and the append-only history.
        "case_state.json" => CaseState,
        "case_event.json" => CaseEvent,
        // Backend selection.
        "backend_capabilities.json" => BackendCapabilities,
        // Egress: what a destination receives and returns.
        "delivery_envelope.json" => DeliveryEnvelope,
        "delivery_receipt.json" => DeliveryReceipt,
        // Failure: the uniform local API error body.
        "api_error.json" => ApiError,
    };

    for (file, schema) in schemas {
        let path = out_dir.join(file);
        // Pretty-printed with a trailing newline so the committed files are
        // reviewable in a diff rather than one unreadable line.
        let mut rendered =
            serde_json::to_string_pretty(&schema).map_err(|e| format!("rendering {file}: {e}"))?;
        rendered.push('\n');
        std::fs::write(&path, rendered).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }

    // A machine-readable statement of which contract version these files
    // describe, so a client can check compatibility without parsing a schema.
    let manifest = serde_json::json!({
        "contract_version": CONTRACT_VERSION,
        "generator": "refinery-schemas",
    });
    let mut rendered =
        serde_json::to_string_pretty(&manifest).map_err(|e| format!("rendering manifest: {e}"))?;
    rendered.push('\n');
    std::fs::write(out_dir.join("contract.json"), rendered)
        .map_err(|e| format!("writing contract.json: {e}"))?;

    Ok(())
}
