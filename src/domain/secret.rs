//! A string that must never appear in a log line, an error, or a debug dump.
//!
//! Callback tokens and destination credentials arrive inside wire contracts,
//! so the redaction has to live on the type rather than in a logging filter
//! that a future call site might forget to apply. [`SecretString`] serializes
//! as its plain value, because the wire needs the real token, but every
//! human-facing rendering path — `Debug`, `Display`, and the generated schema —
//! shows only a placeholder.

use std::fmt;

use schemars::gen::SchemaGenerator;
use schemars::schema::{InstanceType, Schema, SchemaObject};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The text shown wherever a secret would otherwise be rendered.
pub const REDACTED: &str = "[redacted]";

/// A secret string with redacted `Debug` and `Display`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the secret. Every call site that reaches for this is a place a
    /// credential can escape, so keep them few and close to the transport.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is blank.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl JsonSchema for SecretString {
    fn schema_name() -> String {
        "SecretString".to_owned()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        let mut schema = SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            ..Default::default()
        };
        schema.metadata().description = Some(
            "A credential. Accepted on submission and never returned, logged, or displayed."
                .to_owned(),
        );
        schema
            .extensions
            .insert("writeOnly".to_owned(), true.into());
        Schema::Object(schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_a_secret_never_shows_it() {
        let secret = SecretString::new("super-secret-token");
        assert_eq!(format!("{secret}"), REDACTED);
        assert_eq!(format!("{secret:?}"), REDACTED);
        assert!(!format!("{secret:#?}").contains("super-secret-token"));
    }

    #[test]
    fn a_struct_holding_a_secret_redacts_it_too() {
        #[derive(Debug)]
        struct Callback {
            // Only ever read through `Debug`, which is the point of the test.
            #[allow(dead_code)]
            token: SecretString,
        }
        let callback = Callback {
            token: SecretString::new("abc123"),
        };
        assert!(!format!("{callback:?}").contains("abc123"));
    }

    #[test]
    fn the_wire_still_carries_the_real_value() {
        let secret = SecretString::new("abc123");
        assert_eq!(serde_json::to_string(&secret).unwrap(), "\"abc123\"");
        let parsed: SecretString = serde_json::from_str("\"abc123\"").unwrap();
        assert_eq!(parsed.expose(), "abc123");
    }
}
