//! Sanitization for recorded provider fixtures.
//!
//! Provider captures are useful precisely because they contain real protocol
//! shapes, but raw captures may also contain credentials in headers, URLs, or
//! error bodies. Recording therefore always parses and scrubs before it writes
//! a byte to a committed fixture. The companion check applies the same scrubber
//! to existing fixtures and refuses any file that would change.

use serde_json::Value;

use super::redaction;

/// Explicit opt-in required before the fixture recorder writes a file.
pub const RECORD_ENV: &str = "REFINERY_RECORD_FIXTURES";

/// Environment variables whose values should be registered with the literal
/// redactor before a live capture is sanitized.
const SECRET_ENVIRONMENTS: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "REFINERY_API_TOKEN",
    "OVERLORD_TOKEN",
];

/// Whether the process was explicitly placed in record mode.
pub fn record_mode_enabled() -> bool {
    std::env::var(RECORD_ENV).as_deref() == Ok("1")
}

/// Register live credentials that may occur in a provider capture.
pub fn register_recording_secrets() {
    for name in SECRET_ENVIRONMENTS {
        if let Ok(value) = std::env::var(name) {
            redaction::register_secret(&value);
        }
    }
}

/// Parse, recursively scrub, and render a provider JSON capture.
pub fn sanitize_document(input: &str) -> Result<String, String> {
    let mut value: Value =
        serde_json::from_str(input).map_err(|error| format!("fixture is not JSON: {error}"))?;
    sanitize_value(&mut value);
    let mut rendered = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("could not render sanitized fixture: {error}"))?;
    rendered.push('\n');
    Ok(rendered)
}

/// Return whether a committed JSON fixture contains nothing the recorder
/// would scrub. Formatting is intentionally ignored; the check is about data.
pub fn is_sanitized(input: &str) -> Result<bool, String> {
    let original: Value =
        serde_json::from_str(input).map_err(|error| format!("fixture is not JSON: {error}"))?;
    let mut scrubbed = original.clone();
    sanitize_value(&mut scrubbed);
    Ok(original == scrubbed)
}

fn sanitize_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if sensitive_key(key) {
                    *value = Value::String(crate::domain::secret::REDACTED.to_owned());
                } else {
                    sanitize_value(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize_value(item);
            }
        }
        Value::String(text) => {
            let scrubbed = redaction::redact(text);
            if scrubbed.as_ref() != text {
                *text = scrubbed.into_owned();
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "key"
            | "apikey"
            | "accesskey"
            | "accesstoken"
            | "refreshtoken"
            | "token"
            | "authorization"
            | "password"
            | "secret"
            | "clientsecret"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_secrets_and_secrets_inside_strings_are_scrubbed() {
        let raw = r#"{
          "headers": {"Authorization": "Bearer opaque-value"},
          "apiKey": "not-a-recognisable-shape",
          "url": "https://example.test/run?key=AIzaExampleLongCredential",
          "body": {"message": "ordinary provider response"}
        }"#;
        let sanitized = sanitize_document(raw).unwrap();
        assert!(!sanitized.contains("opaque-value"));
        assert!(!sanitized.contains("not-a-recognisable-shape"));
        assert!(!sanitized.contains("AIzaExample"));
        assert!(sanitized.contains("ordinary provider response"));
        assert!(is_sanitized(&sanitized).unwrap());
    }

    #[test]
    fn semantic_identifiers_and_placeholders_survive() {
        let raw = r#"{"name":"__NAME__","project":"demo","status":"ACTIVE"}"#;
        let sanitized = sanitize_document(raw).unwrap();
        assert!(sanitized.contains("__NAME__"));
        assert!(sanitized.contains("demo"));
        assert!(is_sanitized(&sanitized).unwrap());
    }

    #[test]
    fn malformed_captures_are_refused_before_a_file_can_be_written() {
        assert!(sanitize_document("not json").is_err());
        assert!(is_sanitized("not json").is_err());
    }
}
