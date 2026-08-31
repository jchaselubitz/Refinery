//! Installed quality-tool behavior: capture gating, record-time scrubbing, and
//! the versioned evaluation report.

use std::process::Command;

#[test]
fn fixture_recording_requires_explicit_opt_in() {
    let temp = tempfile::tempdir().expect("temp dir");
    let raw = temp.path().join("raw.json");
    let output = temp.path().join("recorded.json");
    std::fs::write(&raw, r#"{"apiKey":"opaque-provider-secret"}"#).expect("write raw");

    let result = Command::new(env!("CARGO_BIN_EXE_refinery-fixtures"))
        .args(["record"])
        .arg(&raw)
        .arg(&output)
        .env_remove("REFINERY_RECORD_FIXTURES")
        .output()
        .expect("run recorder");
    assert!(!result.status.success());
    assert!(!output.exists(), "the disabled recorder opened its output");
    assert!(String::from_utf8_lossy(&result.stderr).contains("record mode is disabled"));
}

#[test]
fn fixture_recording_scrubs_before_the_destination_is_written() {
    let temp = tempfile::tempdir().expect("temp dir");
    let raw = temp.path().join("raw.json");
    let output = temp.path().join("recorded.json");
    let secret = "AIzaLiveCaptureSecretValue123456";
    std::fs::write(
        &raw,
        format!(
            r#"{{"headers":{{"authorization":"Bearer {secret}"}},"error":"provider echoed {secret}","ordinary":"kept"}}"#
        ),
    )
    .expect("write raw");

    let result = Command::new(env!("CARGO_BIN_EXE_refinery-fixtures"))
        .args(["record"])
        .arg(&raw)
        .arg(&output)
        .env("REFINERY_RECORD_FIXTURES", "1")
        .env("GEMINI_API_KEY", secret)
        .output()
        .expect("run recorder");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let recorded = std::fs::read_to_string(output).expect("read recorded fixture");
    assert!(!recorded.contains(secret));
    assert!(recorded.contains("[redacted]"));
    assert!(recorded.contains("kept"));
    serde_json::from_str::<serde_json::Value>(&recorded).expect("recorded fixture is JSON");
}

#[test]
fn the_installed_evaluator_reports_the_versioned_baseline() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluations/v1");
    let output = Command::new(env!("CARGO_BIN_EXE_refinery-eval"))
        .args(["--set"])
        .arg(root)
        .output()
        .expect("run evaluator");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout).expect("utf-8 report");
    assert!(report.contains("evaluation set v1"));
    assert!(report.contains("bounded_http_retries"));
    assert!(report.contains("untrusted_repository_instruction"));
    assert!(report.contains("PASS"));
}
