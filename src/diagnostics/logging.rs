//! Structured logging to a rotating file inside the data directory plus
//! stderr.
//!
//! Log records carry correlation identifiers (case and operation) but never
//! prompts, attachment bodies, repository contents, or secrets. That exclusion
//! is a rule for call sites: the subscriber records whatever it is given, so
//! sensitive values must never be placed in a span or event field.
//!
//! That rule is enforced rather than merely stated: both writers are wrapped
//! in [`redaction::RedactingMakeWriter`], so a formatted record is scrubbed of
//! known and recognisable credentials on its way to the terminal or the file.
//! A call site that leaks a key by accident cannot leak it into a log.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::settings::LoggingSettings;
use crate::config::DataDir;
use crate::diagnostics::redaction::RedactingMakeWriter;
use crate::error::{AppError, Result};

/// Environment variable that overrides the configured log filter.
pub const LOG_FILTER_ENV: &str = "REFINERY_LOG";

/// Keeps the background log writer alive. Dropping it flushes and stops the
/// file writer, so the caller must hold it for the life of the process.
#[must_use = "dropping the guard stops file logging"]
pub struct LoggingHandle {
    _guard: WorkerGuard,
}

/// Install the process-wide subscriber: human-readable or JSON records to
/// stderr, and the same records to a daily-rotating file in the data
/// directory's `logs/` folder.
///
/// Returns an error if a subscriber is already installed, which would
/// otherwise silently drop one of the two configurations.
pub fn init(data_dir: &DataDir, settings: &LoggingSettings) -> Result<LoggingHandle> {
    let log_dir = data_dir.log_dir();
    crate::config::paths::ensure_private_dir(&log_dir)?;

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("refinery")
        .filename_suffix("log")
        .max_log_files(settings.retained_files.max(1))
        .build(&log_dir)
        .map_err(|source| {
            AppError::config(format!(
                "could not open the log directory {}: {source}",
                log_dir.display()
            ))
        })?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let stderr_writer = RedactingMakeWriter::new(std::io::stderr);
    let file_writer = RedactingMakeWriter::new(file_writer);

    let (stderr_layer, file_layer) = if settings.json {
        (
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(stderr_writer.clone())
                .boxed(),
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(file_writer.clone())
                .boxed(),
        )
    } else {
        (
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(stderr_writer.clone())
                .boxed(),
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(file_writer.clone())
                .boxed(),
        )
    };

    tracing_subscriber::registry()
        .with(filter(&settings.filter)?)
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|source| AppError::config(format!("logging was already initialized: {source}")))?;

    Ok(LoggingHandle { _guard: guard })
}

/// Build the filter from `REFINERY_LOG` when set, otherwise from settings.
fn filter(configured: &str) -> Result<EnvFilter> {
    match std::env::var(LOG_FILTER_ENV) {
        Ok(value) if !value.trim().is_empty() => EnvFilter::try_new(&value).map_err(|source| {
            AppError::config(format!(
                "{LOG_FILTER_ENV} is not a valid log filter: {source}"
            ))
        }),
        _ => EnvFilter::try_new(configured).map_err(|source| {
            AppError::config(format!(
                "logging.filter is not a valid log filter: {source}"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_filter_is_accepted() {
        filter("refinery=debug,info").expect("valid directive");
    }

    #[test]
    fn an_invalid_filter_is_a_config_error() {
        let error = filter("refinery=notalevel").expect_err("invalid directive");
        assert_eq!(error.code(), "config_error");
    }
}
