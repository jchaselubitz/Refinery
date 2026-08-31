//! Keeping credentials out of logs, errors, and terminal output.
//!
//! Two mechanisms work together, because either alone leaks.
//!
//! The registry holds the literal secrets this process actually knows. Every
//! value read out of the credential store is registered, so a log line that
//! interpolates a key — whatever the call site was trying to say — has that
//! key replaced before the bytes reach a file or a terminal. This is exact and
//! cannot produce a false positive.
//!
//! The scanner catches secrets this process never held: a provider's own error
//! body echoing the key back, a URL with a `key=` query parameter, an
//! `Authorization` header captured in a diagnostic dump. It looks only for
//! shapes that are unambiguously credentials, because a redactor that eats
//! ordinary prose makes logs useless and gets turned off.
//!
//! Redaction is applied at the writer, not at the call site. A `tracing` field
//! is formatted into a line and the line is scrubbed on its way out, so a new
//! call site added later is covered without anyone remembering to think about
//! it.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io;
use std::sync::{OnceLock, RwLock};

use crate::domain::secret::REDACTED;

/// Secrets shorter than this are not registered. A three-character "secret"
/// would match half the words in a log file and redact them all.
const MIN_REGISTERED_LEN: usize = 8;

fn registry() -> &'static RwLock<BTreeSet<String>> {
    static REGISTRY: OnceLock<RwLock<BTreeSet<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(BTreeSet::new()))
}

/// Register a live secret so every later log line and error message replaces
/// it with a placeholder.
///
/// Call this wherever a credential enters the process. Registering the same
/// value twice is harmless. Values that are blank or implausibly short are
/// ignored rather than turning common substrings into `[redacted]`.
pub fn register_secret(value: &str) {
    if value.trim().len() < MIN_REGISTERED_LEN {
        return;
    }
    if let Ok(mut registry) = registry().write() {
        registry.insert(value.to_owned());
    }
}

/// Forget every registered secret. Used by tests and by credential deletion.
pub fn clear_registered_secrets() {
    if let Ok(mut registry) = registry().write() {
        registry.clear();
    }
}

/// Replace every known or recognisable credential in `text`.
///
/// Returns the input untouched when nothing matched, so the common case of a
/// clean log line costs no allocation.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = redact_registered(text);
    if let Some(scanned) = scan_for_credentials(out.as_ref()) {
        out = Cow::Owned(scanned);
    }
    out
}

fn redact_registered(text: &str) -> Cow<'_, str> {
    let Ok(registry) = registry().read() else {
        return Cow::Borrowed(text);
    };
    let mut current = Cow::Borrowed(text);
    for secret in registry.iter() {
        if current.contains(secret.as_str()) {
            current = Cow::Owned(current.replace(secret.as_str(), REDACTED));
        }
    }
    current
}

/// Prefixes that identify a credential by themselves. A token beginning with
/// one of these is a secret regardless of where it came from.
const SECRET_TOKEN_PREFIXES: &[&str] = &[
    // Google API keys, which is what a Gemini key is.
    "AIza", // Refinery's own local bearer tokens.
    "rfy_",
];

/// Keys whose value is a credential when they appear as `name=value`, in a URL
/// query or a serialised structure.
const SECRET_ASSIGNMENT_KEYS: &[&str] = &[
    "key",
    "api_key",
    "apikey",
    "access_token",
    "token",
    "authorization",
    "password",
    "secret",
];

/// Scan for credential shapes the registry cannot know about.
///
/// Returns `None` when the text is already clean, which is the usual answer.
fn scan_for_credentials(text: &str) -> Option<String> {
    let mut out: Option<String> = None;
    // The index of the first byte not yet copied into `out`. Copying in runs
    // rather than per character keeps the untouched text exact, multi-byte
    // characters included.
    let mut copied_to = 0;
    let mut index = 0;

    while index < text.len() {
        if let Some(span) = credential_at(text, index) {
            let buffer = out.get_or_insert_with(String::new);
            buffer.push_str(&text[copied_to..span.value_start]);
            buffer.push_str(REDACTED);
            copied_to = span.value_end;
            index = span.value_end;
            continue;
        }
        index += next_char_len(text.as_bytes(), index);
    }

    if let Some(buffer) = out.as_mut() {
        buffer.push_str(&text[copied_to..]);
    }
    out
}

/// Where a detected credential's value begins and ends.
struct CredentialSpan {
    value_start: usize,
    value_end: usize,
}

fn credential_at(text: &str, index: usize) -> Option<CredentialSpan> {
    if !is_token_boundary(text, index) {
        return None;
    }

    for prefix in SECRET_TOKEN_PREFIXES {
        if text[index..].starts_with(prefix) {
            let end = token_end(text, index);
            // A bare prefix with nothing after it is not a credential.
            if end > index + prefix.len() {
                return Some(CredentialSpan {
                    value_start: index,
                    value_end: end,
                });
            }
        }
    }

    // `Bearer <token>` in an Authorization header.
    if let Some(rest) = strip_prefix_ignore_case(&text[index..], "bearer ") {
        let value_start = index + (text.len() - index - rest.len());
        let value_start = value_start + leading_spaces(&text[value_start..]);
        let end = token_end(text, value_start);
        if end > value_start {
            return Some(CredentialSpan {
                value_start,
                value_end: end,
            });
        }
    }

    // `key=value`, `token: value`, and friends.
    for name in SECRET_ASSIGNMENT_KEYS {
        let Some(rest) = strip_prefix_ignore_case(&text[index..], name) else {
            continue;
        };
        let after_name = text.len() - rest.len();
        let separator = rest.trim_start_matches([' ', '"', '\'']);
        let Some(separator_char) = separator.chars().next() else {
            continue;
        };
        if !matches!(separator_char, '=' | ':') {
            continue;
        }
        let value_start = after_name + (rest.len() - separator.len()) + separator_char.len_utf8();
        let value_start = value_start + leading_value_padding(&text[value_start..]);
        let end = token_end(text, value_start);
        // Short values are configuration, not credentials: `port=8787` and
        // `key: id` must survive.
        if end.saturating_sub(value_start) >= MIN_REGISTERED_LEN {
            return Some(CredentialSpan {
                value_start,
                value_end: end,
            });
        }
    }

    None
}

fn is_token_boundary(text: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    match text[..index].chars().next_back() {
        Some(previous) => !previous.is_ascii_alphanumeric() && previous != '_',
        None => true,
    }
}

fn token_end(text: &str, start: usize) -> usize {
    text[start..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+')))
        .map_or(text.len(), |offset| start + offset)
}

fn leading_spaces(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

fn leading_value_padding(text: &str) -> usize {
    text.len() - text.trim_start_matches([' ', '"', '\'']).len()
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

fn next_char_len(bytes: &[u8], index: usize) -> usize {
    let first = bytes[index];
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// A writer that scrubs credentials out of every line it is handed.
///
/// `tracing` hands a formatted record to the writer as one `write` call, so
/// redacting here covers text, JSON, spans, and error chains alike without any
/// awareness of the log format.
pub struct RedactingWriter<W> {
    inner: W,
}

impl<W: io::Write> RedactingWriter<W> {
    /// Wrap a writer so its output is redacted.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Non-UTF-8 output cannot be scanned for credentials, but it also
        // cannot be a log line Refinery produced, so it passes through.
        match std::str::from_utf8(buf) {
            Ok(text) => {
                self.inner.write_all(redact(text).as_bytes())?;
                Ok(buf.len())
            }
            Err(_) => self.inner.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A [`tracing_subscriber::fmt::MakeWriter`] that redacts everything written
/// through it.
#[derive(Clone, Debug)]
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    /// Wrap a writer factory.
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<'a, M> tracing_subscriber::fmt::MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: tracing_subscriber::fmt::MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer())
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer_for(meta))
    }
}

/// Render an error for a person, with credentials removed.
///
/// Every path that prints an error to a terminal or puts one in an API body
/// goes through this rather than `Display` directly.
pub fn redact_error(error: &dyn std::error::Error) -> String {
    redact(&error.to_string()).into_owned()
}

/// Render an application error's detail, with credentials removed.
///
/// Used where the surrounding output already states the status, so the
/// variant prefix `Display` adds would be redundant.
pub fn redact_detail(error: &crate::error::AppError) -> String {
    redact(&error.detail()).into_owned()
}

/// Test-only serialisation of the process-wide secret registry.
///
/// The registry is global by design — a redactor that only covered part of the
/// process would not be a redactor — so tests that register and clear secrets
/// must not run against each other. Every test that touches the registry takes
/// this lock.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Hold the registry for the duration of one test.
    pub(crate) fn registry_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register a secret, run the body, then clear the registry.
    pub(crate) fn with_registered<T>(secret: &str, body: impl FnOnce() -> T) -> T {
        let _guard = registry_lock();
        super::clear_registered_secrets();
        super::register_secret(secret);
        let result = body();
        super::clear_registered_secrets();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{registry_lock, with_registered};
    use super::*;

    #[test]
    fn a_registered_secret_never_survives_a_log_line() {
        with_registered("s3cret-value-not-in-prose", || {
            let line = redact("provider call failed with s3cret-value-not-in-prose in the url");
            assert!(!line.contains("s3cret-value-not-in-prose"));
            assert!(line.contains(REDACTED));
        });
    }

    #[test]
    fn clean_text_is_returned_untouched() {
        assert!(matches!(
            redact("prepared case 018f-...-a1 in 12ms"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_google_api_key_is_caught_without_being_registered() {
        let line = redact("GET https://generativelanguage.googleapis.com/v1beta/models?key=AIzaSyD-ExampleExampleExampleExample1");
        assert!(!line.contains("AIzaSyD"));
        assert!(line.contains(REDACTED));
    }

    #[test]
    fn a_bearer_header_is_caught() {
        let line = redact("authorization: Bearer rfy_abcdefghijklmnopqrstuvwxyz");
        assert!(!line.contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn assignment_syntax_is_caught_in_any_case() {
        for line in [
            "api_key=\"sk-abcdefghijklmnop\"",
            "API_KEY: sk-abcdefghijklmnop",
            "{\"token\": \"sk-abcdefghijklmnop\"}",
        ] {
            assert!(
                !redact(line).contains("sk-abcdefghijklmnop"),
                "leaked from {line}"
            );
        }
    }

    #[test]
    fn ordinary_configuration_output_is_not_mangled() {
        // A redactor that eats these makes `refinery status` unreadable and
        // gets switched off, so the false-positive case is a real test.
        for line in [
            "api             http://127.0.0.1:8787",
            "provider        gemini (gemini-3.7-flash)",
            "key: id",
            "port=8787",
            "the key is stored in the OS credential store",
            "data directory  /Users/someone/Library/Application Support/Refinery",
        ] {
            assert_eq!(redact(line), line, "mangled {line}");
        }
    }

    #[test]
    fn short_values_are_not_treated_as_secrets() {
        let _guard = registry_lock();
        register_secret("abc");
        let line = redact("abc appears in ordinary prose");
        clear_registered_secrets();
        assert_eq!(line, "abc appears in ordinary prose");
    }

    #[test]
    fn the_writer_scrubs_what_passes_through_it() {
        use std::io::Write;

        with_registered("registered-secret-value", || {
            let mut buffer = Vec::new();
            {
                let mut writer = RedactingWriter::new(&mut buffer);
                writer
                    .write_all(b"key registered-secret-value used\n")
                    .expect("write");
            }
            let written = String::from_utf8(buffer).expect("utf8");
            assert!(!written.contains("registered-secret-value"));
            assert!(written.ends_with('\n'));
        });
    }

    #[test]
    fn multibyte_text_survives_scanning() {
        let line =
            redact("prepared café — naïve — 東京 with AIzaSyD-ExampleExampleExampleExample1");
        assert!(line.contains("café"));
        assert!(line.contains("東京"));
        assert!(!line.contains("AIzaSyD"));
    }

    #[test]
    fn errors_are_rendered_redacted() {
        with_registered("leaky-token-value", || {
            let error = crate::error::AppError::config("could not use leaky-token-value");
            assert!(!redact_error(&error).contains("leaky-token-value"));
        });
    }
}
