//! Where secrets live.
//!
//! The provider API key and the local bearer token are the only secrets
//! Refinery holds, and neither is ever written to the settings file. The
//! preferred home is the operating system's credential store: the Keychain on
//! macOS, the Credential Manager on Windows, and the Secret Service on Linux
//! desktops.
//!
//! Linux servers, containers, and headless CI have no Secret Service, and a
//! product that refuses to run there is not usable. Where no store answers,
//! Refinery falls back to a `0600` file inside the data directory and says so
//! — at setup, in `status`, and as a `doctor` warning — because a secret in a
//! file the user's backup tool might copy is a real, if accepted, downgrade.
//!
//! Which backend is in use is decided once, at open, by probing the store. A
//! per-operation decision would let a store that appears mid-session split the
//! secrets across two homes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::paths::set_private_file_mode;
use crate::diagnostics::redaction;
use crate::domain::secret::SecretString;
use crate::error::{AppError, Result};

/// The service name Refinery registers under in the OS credential store.
pub const KEYRING_SERVICE: &str = "io.cooperativ.refinery";

/// File name of the fallback credential file.
pub const FALLBACK_FILE: &str = "credentials.json";

/// Environment variable that forces a credential backend.
///
/// Set it to `file` to keep secrets in the data directory even where an
/// operating-system store exists. Two reasons this exists rather than being
/// left to auto-detection: a user whose keyring is broken or perpetually
/// prompting needs a way to proceed, and the test suite must never read or
/// write the credential store of the machine running it. Any other value is
/// ignored, so a typo falls back to detection rather than losing the secret.
pub const CREDENTIAL_BACKEND_ENV: &str = "REFINERY_CREDENTIALS";

/// Which secret is being read or written.
///
/// Credentials are addressed by a typed key rather than a free string so that
/// a caller cannot invent an account name that another build spells
/// differently and silently lose the user's key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CredentialKey {
    /// The API key for a model provider, for example `gemini`.
    ProviderApiKey {
        /// The provider identifier.
        provider: String,
    },
    /// The bearer token local clients present to the loopback API.
    ApiBearerToken,
}

impl CredentialKey {
    /// The API key for a provider.
    pub fn provider_api_key(provider: impl Into<String>) -> Self {
        Self::ProviderApiKey {
            provider: provider.into(),
        }
    }

    /// The stable account name used in the OS store and the fallback file.
    ///
    /// Changing one of these strings orphans a user's stored secret, so they
    /// are part of the installed format.
    pub fn account(&self) -> String {
        match self {
            Self::ProviderApiKey { provider } => format!("provider.{provider}.api_key"),
            Self::ApiBearerToken => "api.bearer_token".to_owned(),
        }
    }

    /// How the key is described to a person.
    pub fn describe(&self) -> String {
        match self {
            Self::ProviderApiKey { provider } => format!("{provider} API key"),
            Self::ApiBearerToken => "local API token".to_owned(),
        }
    }
}

/// Where secrets are actually being kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialBackend {
    /// The operating system's credential store.
    OperatingSystem,
    /// An owner-only file inside the data directory, used where the platform
    /// exposes no credential store.
    FallbackFile,
}

impl CredentialBackend {
    /// Whether this backend warrants a warning to the user.
    pub fn is_fallback(self) -> bool {
        matches!(self, Self::FallbackFile)
    }

    /// A short description for status output.
    pub fn label(self) -> &'static str {
        match self {
            Self::OperatingSystem => "operating-system credential store",
            Self::FallbackFile => "owner-only file (no OS credential store)",
        }
    }
}

impl std::fmt::Display for CredentialBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The secret store for one installation.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    backend: CredentialBackend,
    fallback_path: PathBuf,
    /// Why the OS store was unavailable, kept so `doctor` can explain the
    /// fallback rather than only announcing it.
    unavailable_reason: Option<String>,
}

impl CredentialStore {
    /// Open the credential store for a data directory, probing the operating
    /// system store once and falling back to a private file if none answers.
    pub fn open(credentials_dir: &Path) -> Self {
        if forced_to_file() {
            let mut store = Self::file_backed(credentials_dir);
            store.unavailable_reason = Some(format!("{CREDENTIAL_BACKEND_ENV} is set to `file`"));
            return store;
        }
        let fallback_path = credentials_dir.join(FALLBACK_FILE);
        match probe_os_store() {
            Ok(()) => Self {
                backend: CredentialBackend::OperatingSystem,
                fallback_path,
                unavailable_reason: None,
            },
            Err(reason) => Self {
                backend: CredentialBackend::FallbackFile,
                fallback_path,
                unavailable_reason: Some(reason),
            },
        }
    }

    /// Open a store pinned to the fallback file, ignoring any OS store.
    ///
    /// Tests use this so they never touch the developer's real Keychain, and
    /// it gives a user on a machine with a broken store a way to proceed.
    pub fn file_backed(credentials_dir: &Path) -> Self {
        Self {
            backend: CredentialBackend::FallbackFile,
            fallback_path: credentials_dir.join(FALLBACK_FILE),
            unavailable_reason: Some("pinned to the fallback file".to_owned()),
        }
    }

    /// Which backend this store writes to.
    pub fn backend(&self) -> CredentialBackend {
        self.backend
    }

    /// Why the operating-system store was not used, when it was not.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }

    /// The path of the fallback file, whether or not it is in use.
    pub fn fallback_path(&self) -> &Path {
        &self.fallback_path
    }

    /// Store a secret, replacing any existing value for the key.
    pub fn set(&self, key: &CredentialKey, secret: &SecretString) -> Result<()> {
        if secret.expose().trim().is_empty() {
            return Err(AppError::invalid(format!(
                "the {} must not be blank",
                key.describe()
            )));
        }
        match self.backend {
            CredentialBackend::OperatingSystem => {
                let entry = entry_for(key)?;
                entry.set_password(secret.expose()).map_err(|source| {
                    AppError::config(format!(
                        "could not store the {} in the operating-system credential store: {source}",
                        key.describe()
                    ))
                })?;
            }
            CredentialBackend::FallbackFile => {
                let mut file = FallbackFile::load(&self.fallback_path)?;
                file.secrets
                    .insert(key.account(), secret.expose().to_owned());
                file.save(&self.fallback_path)?;
            }
        }
        redaction::register_secret(secret.expose());
        Ok(())
    }

    /// Read a secret, returning `None` when it has never been stored.
    ///
    /// Every secret read here is registered with the redaction layer, so a
    /// credential cannot reach a log line simply because a later call site
    /// forgot about it.
    pub fn get(&self, key: &CredentialKey) -> Result<Option<SecretString>> {
        let raw = match self.backend {
            CredentialBackend::OperatingSystem => match entry_for(key)?.get_password() {
                Ok(value) => Some(value),
                Err(keyring::Error::NoEntry) => None,
                Err(source) => {
                    return Err(AppError::config(format!(
                    "could not read the {} from the operating-system credential store: {source}",
                    key.describe()
                )))
                }
            },
            CredentialBackend::FallbackFile => FallbackFile::load(&self.fallback_path)?
                .secrets
                .remove(&key.account()),
        };

        Ok(raw.map(|value| {
            redaction::register_secret(&value);
            SecretString::new(value)
        }))
    }

    /// Remove a secret. Removing an absent secret succeeds, because the
    /// caller's intent — that the secret not be there — is satisfied.
    pub fn delete(&self, key: &CredentialKey) -> Result<()> {
        match self.backend {
            CredentialBackend::OperatingSystem => match entry_for(key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(source) => Err(AppError::config(format!(
                    "could not remove the {} from the operating-system credential store: {source}",
                    key.describe()
                ))),
            },
            CredentialBackend::FallbackFile => {
                let mut file = FallbackFile::load(&self.fallback_path)?;
                if file.secrets.remove(&key.account()).is_some() {
                    file.save(&self.fallback_path)?;
                }
                Ok(())
            }
        }
    }

    /// Whether a secret is present, without materialising it.
    pub fn contains(&self, key: &CredentialKey) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }
}

/// Whether the environment pins the store to the fallback file.
fn forced_to_file() -> bool {
    std::env::var(CREDENTIAL_BACKEND_ENV)
        .map(|value| value.trim().eq_ignore_ascii_case("file"))
        .unwrap_or(false)
}

fn entry_for(key: &CredentialKey) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &key.account()).map_err(|source| {
        AppError::config(format!(
            "could not address the {} in the operating-system credential store: {source}",
            key.describe()
        ))
    })
}

/// Ask the platform whether it has a usable credential store.
///
/// The probe reads an entry that is never written. A store that is present and
/// working answers "no such entry"; a platform without one fails to build or
/// reach the store at all, and that failure is what selects the fallback.
fn probe_os_store() -> std::result::Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, "availability.probe")
        .map_err(|source| source.to_string())?;
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(source) => Err(source.to_string()),
    }
}

/// The on-disk shape of the fallback credential file.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FallbackFile {
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

impl FallbackFile {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| {
                AppError::config(format!(
                    "{} is not a readable credential file: {source}",
                    path.display()
                ))
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(AppError::io(path, source)),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            crate::config::paths::ensure_private_dir(parent)?;
        }
        // The file is created before the bytes are written so the restrictive
        // mode is in place first; writing then tightening would leave a window
        // in which the secret is world-readable.
        if !path.exists() {
            std::fs::write(path, "{}").map_err(|source| AppError::io(path, source))?;
            set_private_file_mode(path)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|source| {
            AppError::config(format!("could not encode the credential file: {source}"))
        })?;
        std::fs::write(path, text).map_err(|source| AppError::io(path, source))?;
        set_private_file_mode(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(temp: &tempfile::TempDir) -> CredentialStore {
        CredentialStore::file_backed(temp.path())
    }

    #[test]
    fn accounts_are_stable_and_distinct() {
        assert_eq!(
            CredentialKey::provider_api_key("gemini").account(),
            "provider.gemini.api_key"
        );
        assert_ne!(
            CredentialKey::ApiBearerToken.account(),
            CredentialKey::provider_api_key("gemini").account()
        );
    }

    #[test]
    fn a_secret_round_trips_through_the_fallback_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = store(&temp);
        let key = CredentialKey::provider_api_key("gemini");

        assert!(store.get(&key).expect("read absent").is_none());
        store
            .set(&key, &SecretString::new("AIzaTestKeyValue0123456789"))
            .expect("store");
        assert_eq!(
            store.get(&key).expect("read").expect("present").expose(),
            "AIzaTestKeyValue0123456789"
        );

        store.delete(&key).expect("delete");
        assert!(store.get(&key).expect("read after delete").is_none());
        // Deleting again is not an error: the intent is already satisfied.
        store.delete(&key).expect("idempotent delete");
    }

    #[test]
    fn distinct_keys_do_not_overwrite_each_other() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = store(&temp);
        store
            .set(
                &CredentialKey::provider_api_key("gemini"),
                &SecretString::new("gemini-key-value-0123"),
            )
            .expect("store provider key");
        store
            .set(
                &CredentialKey::ApiBearerToken,
                &SecretString::new("rfy_local_token_value"),
            )
            .expect("store bearer token");

        assert_eq!(
            store
                .get(&CredentialKey::provider_api_key("gemini"))
                .expect("read")
                .expect("present")
                .expose(),
            "gemini-key-value-0123"
        );
        assert_eq!(
            store
                .get(&CredentialKey::ApiBearerToken)
                .expect("read")
                .expect("present")
                .expose(),
            "rfy_local_token_value"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let store = store(&temp);
        store
            .set(
                &CredentialKey::ApiBearerToken,
                &SecretString::new("rfy_local_token_value"),
            )
            .expect("store");

        let mode = std::fs::metadata(store.fallback_path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, crate::config::paths::FILE_MODE);
    }

    #[test]
    fn a_blank_secret_is_refused_rather_than_stored() {
        let temp = tempfile::tempdir().expect("temp dir");
        let error = store(&temp)
            .set(&CredentialKey::ApiBearerToken, &SecretString::new("   "))
            .expect_err("a blank secret is not a secret");
        assert_eq!(error.code(), "invalid_request");
    }

    #[test]
    fn a_corrupt_credential_file_is_reported_rather_than_discarded() {
        let temp = tempfile::tempdir().expect("temp dir");
        std::fs::write(temp.path().join(FALLBACK_FILE), "not json").expect("write");
        let error = store(&temp)
            .get(&CredentialKey::ApiBearerToken)
            .expect_err("unreadable file must not look like an absent secret");
        assert_eq!(error.code(), "config_error");
    }

    #[test]
    fn a_stored_secret_is_registered_for_redaction() {
        let _guard = redaction::test_support::registry_lock();
        let temp = tempfile::tempdir().expect("temp dir");
        let store = store(&temp);
        store
            .set(
                &CredentialKey::provider_api_key("gemini"),
                &SecretString::new("redaction-registration-probe"),
            )
            .expect("store");

        let line = redaction::redact("calling gemini with redaction-registration-probe");
        let clean = !line.contains("redaction-registration-probe");
        redaction::clear_registered_secrets();
        assert!(clean, "a stored secret must be registered for redaction");
    }

    #[test]
    fn the_environment_can_pin_the_store_to_the_fallback_file() {
        // Setting a process-wide variable makes this test order-dependent with
        // any other that reads it, so it is the only test that touches it and
        // it restores the previous value.
        let temp = tempfile::tempdir().expect("temp dir");
        let previous = std::env::var_os(CREDENTIAL_BACKEND_ENV);
        std::env::set_var(CREDENTIAL_BACKEND_ENV, "file");
        let store = CredentialStore::open(temp.path());
        match previous {
            Some(value) => std::env::set_var(CREDENTIAL_BACKEND_ENV, value),
            None => std::env::remove_var(CREDENTIAL_BACKEND_ENV),
        }

        assert_eq!(store.backend(), CredentialBackend::FallbackFile);
        assert!(store
            .unavailable_reason()
            .expect("reason")
            .contains(CREDENTIAL_BACKEND_ENV));
    }

    #[test]
    fn the_fallback_backend_is_reported_as_a_fallback() {
        let temp = tempfile::tempdir().expect("temp dir");
        let store = store(&temp);
        assert!(store.backend().is_fallback());
        assert!(store.unavailable_reason().is_some());
    }
}
