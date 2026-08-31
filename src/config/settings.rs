//! The non-secret settings file.
//!
//! Everything here is safe to read in plain text. Credentials live in the
//! operating-system credential store and are never written to this file; the
//! settings only record which provider is selected and how the local service
//! is bound and bounded.

use std::net::Ipv4Addr;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::paths::set_private_file_mode;
use crate::error::{AppError, Result};

/// Version of the settings file format, so a future migration can detect an
/// older file rather than silently misreading it.
pub const SETTINGS_VERSION: u32 = 1;

/// The non-secret application settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Settings file format version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Local HTTP API settings.
    #[serde(default)]
    pub api: ApiSettings,
    /// Logging settings.
    #[serde(default)]
    pub logging: LoggingSettings,
    /// Model-provider selection.
    #[serde(default)]
    pub provider: ProviderSettings,
    /// Bounds applied to inputs and repository reads.
    #[serde(default)]
    pub limits: LimitSettings,
    /// The local Overlord destination, when one is configured.
    #[serde(default)]
    pub overlord: OverlordSettings,
}

/// Local HTTP API settings. The listener is loopback-only by construction:
/// the address is not configurable, only the port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSettings {
    /// The loopback port the daemon listens on.
    #[serde(default = "default_port")]
    pub port: u16,
}

/// Logging settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingSettings {
    /// A `tracing` filter directive, for example `info` or `refinery=debug`.
    #[serde(default = "default_log_filter")]
    pub filter: String,
    /// Emit newline-delimited JSON instead of human-readable text.
    #[serde(default)]
    pub json: bool,
    /// How many rotated log files to keep.
    #[serde(default = "default_log_files")]
    pub retained_files: usize,
}

/// Which model provider backs refinement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    /// The selected backend identifier. Stage 1 ships `gemini` only.
    #[serde(default = "default_backend")]
    pub backend: String,
    /// The model identifier passed to the selected backend.
    #[serde(default = "default_model")]
    pub model: String,
}

/// Where a refined prompt is delivered when the destination is Overlord.
///
/// Only the base URL lives here. The token a delivery presents is a credential
/// and lives in the credential store, referenced by name, so this file stays
/// safe to read, copy, and paste into a support thread.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlordSettings {
    /// The base URL of a local Overlord instance, for example
    /// `http://127.0.0.1:3000`. Absent until setup configures one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Bounds applied to inputs, repository reads, and attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitSettings {
    /// Largest transcript accepted on ingress, in bytes.
    #[serde(default = "default_max_transcript_bytes")]
    pub max_transcript_bytes: u64,
    /// Largest single repository file a tool will read, in bytes.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
    /// Largest attachment accepted into the media store, in bytes.
    #[serde(default = "default_max_attachment_bytes")]
    pub max_attachment_bytes: u64,
    /// Largest number of entries any repository tool returns.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

fn default_version() -> u32 {
    SETTINGS_VERSION
}
fn default_port() -> u16 {
    8787
}
fn default_log_filter() -> String {
    "info".to_string()
}
fn default_log_files() -> usize {
    7
}
fn default_backend() -> String {
    "gemini".to_string()
}
fn default_model() -> String {
    "gemini-3.7-flash".to_string()
}
fn default_max_transcript_bytes() -> u64 {
    1_000_000
}
fn default_max_file_bytes() -> u64 {
    512_000
}
fn default_max_attachment_bytes() -> u64 {
    100 * 1024 * 1024
}
fn default_max_results() -> usize {
    200
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: default_version(),
            api: ApiSettings::default(),
            logging: LoggingSettings::default(),
            provider: ProviderSettings::default(),
            limits: LimitSettings::default(),
            overlord: OverlordSettings::default(),
        }
    }
}

impl Default for ApiSettings {
    fn default() -> Self {
        Self {
            port: default_port(),
        }
    }
}

impl ApiSettings {
    /// The socket address the daemon binds. Always loopback.
    pub fn bind_address(&self) -> std::net::SocketAddr {
        (Ipv4Addr::LOCALHOST, self.port).into()
    }
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            filter: default_log_filter(),
            json: false,
            retained_files: default_log_files(),
        }
    }
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            model: default_model(),
        }
    }
}

impl Default for LimitSettings {
    fn default() -> Self {
        Self {
            max_transcript_bytes: default_max_transcript_bytes(),
            max_file_bytes: default_max_file_bytes(),
            max_attachment_bytes: default_max_attachment_bytes(),
            max_results: default_max_results(),
        }
    }
}

impl Settings {
    /// Load settings from `path`, returning defaults when the file is absent.
    ///
    /// A missing file is normal on first run; an unparsable or
    /// future-versioned file is an error rather than a silent reset, because
    /// discarding a user's configuration is worse than refusing to start.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(source) => return Err(AppError::io(path, source)),
        };

        let settings: Settings = toml::from_str(&text).map_err(|source| {
            AppError::config(format!(
                "{} is not valid settings: {source}",
                path.display()
            ))
        })?;
        settings.validate(path)?;
        Ok(settings)
    }

    /// Write settings to `path` with owner-only permissions.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|source| AppError::config(format!("could not encode settings: {source}")))?;
        std::fs::write(path, text).map_err(|source| AppError::io(path, source))?;
        set_private_file_mode(path)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.version > SETTINGS_VERSION {
            return Err(AppError::config(format!(
                "{} was written by a newer Refinery (settings version {}, this build understands {SETTINGS_VERSION}); upgrade Refinery",
                path.display(),
                self.version
            )));
        }
        if self.api.port == 0 {
            return Err(AppError::config(format!(
                "{}: api.port must be a fixed port, not 0",
                path.display()
            )));
        }
        if self.provider.backend.trim().is_empty() {
            return Err(AppError::config(format!(
                "{}: provider.backend must not be empty",
                path.display()
            )));
        }
        if let Some(url) = &self.overlord.base_url {
            validate_overlord_url(url).map_err(|message| {
                AppError::config(format!("{}: overlord.base_url {message}", path.display()))
            })?;
        }
        Ok(())
    }
}

/// Check that an Overlord base URL addresses a local instance.
///
/// Stage 1 delivers to an Overlord on the same machine. Refusing anything else
/// here means a settings file — which is not a secret and may be shared or
/// copied between machines — can never redirect a user's refined prompts to a
/// host they did not intend.
pub fn validate_overlord_url(url: &str) -> std::result::Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|source| format!("is not a URL: {source}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("must be http or https, not `{}`", parsed.scheme()));
    }
    // `host()` is used rather than `host_str()` because the latter keeps the
    // brackets around an IPv6 literal, which then fails to parse as an address.
    let Some(host) = parsed.host() else {
        return Err("must name a host".to_owned());
    };
    let is_loopback = match host {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    };
    if !is_loopback {
        return Err(format!(
            "must address a local Overlord on loopback, not `{host}`"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_overlord_url_is_accepted() {
        for url in [
            "http://127.0.0.1:3000",
            "http://localhost:3000/api",
            "https://[::1]:3000",
        ] {
            validate_overlord_url(url).unwrap_or_else(|error| panic!("{url}: {error}"));
        }
    }

    #[test]
    fn a_remote_overlord_url_is_refused() {
        // A settings file is not a secret and may be copied between machines;
        // it must never be able to redirect refined prompts off the box.
        for url in [
            "http://overlord.example.com",
            "http://10.0.0.5:3000",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(validate_overlord_url(url).is_err(), "{url} was accepted");
        }
    }

    #[test]
    fn a_settings_file_with_a_remote_overlord_refuses_to_load() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("refinery.toml");
        std::fs::write(
            &path,
            "[overlord]\nbase_url = \"http://overlord.example.com\"\n",
        )
        .expect("write");

        let error = Settings::load(&path).expect_err("must refuse");
        assert_eq!(error.code(), "config_error");
    }

    #[test]
    fn an_overlord_url_round_trips() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("refinery.toml");
        let mut settings = Settings::default();
        settings.overlord.base_url = Some("http://127.0.0.1:3000".into());
        settings.save(&path).expect("save");
        assert_eq!(Settings::load(&path).expect("load"), settings);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let temp = tempfile::tempdir().expect("temp dir");
        let settings = Settings::load(&temp.path().join("absent.toml")).expect("load");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("refinery.toml");
        std::fs::write(&path, "[api]\nport = 9001\n").expect("write");

        let settings = Settings::load(&path).expect("load");
        assert_eq!(settings.api.port, 9001);
        assert_eq!(settings.provider.backend, "gemini");
        assert_eq!(settings.limits.max_results, 200);
    }

    #[test]
    fn round_trips_through_the_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("refinery.toml");
        let mut settings = Settings::default();
        settings.logging.json = true;
        settings.api.port = 12345;

        settings.save(&path).expect("save");
        assert_eq!(Settings::load(&path).expect("load"), settings);
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_ignored() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("refinery.toml");
        std::fs::write(&path, "[api]\nprot = 9001\n").expect("write");

        let error = Settings::load(&path).expect_err("typo must not be silently ignored");
        assert_eq!(error.code(), "config_error");
    }

    #[test]
    fn newer_settings_version_refuses_to_load() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("refinery.toml");
        std::fs::write(&path, format!("version = {}\n", SETTINGS_VERSION + 1)).expect("write");

        let error = Settings::load(&path).expect_err("newer file must not load");
        assert_eq!(error.code(), "config_error");
    }

    #[test]
    fn the_api_binds_loopback_only() {
        assert!(ApiSettings::default().bind_address().ip().is_loopback());
    }

    #[cfg(unix)]
    #[test]
    fn saved_settings_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("refinery.toml");
        Settings::default().save(&path).expect("save");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, crate::config::paths::FILE_MODE);
    }
}
