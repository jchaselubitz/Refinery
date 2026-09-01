//! `refinery uninstall` — remove this installation from the machine.
//!
//! An installer that cannot be reversed is an installer people are right to be
//! wary of, so removal is a first-class command rather than a paragraph of
//! `rm` incantations in a README. It undoes exactly what installing and running
//! Refinery put on the machine, in the order that leaves nothing running:
//!
//! 1. stop the background service and delete its unit file, so the supervisor
//!    is not left pointing at a binary that is about to disappear;
//! 2. delete the stored secrets — the provider API key and the loopback bearer
//!    token — which live in the operating-system credential store, outside the
//!    data directory and therefore outside anything a `rm -rf` would reach;
//! 3. with `--purge`, delete the data directory: settings, database, media, and
//!    logs;
//! 4. delete the binary itself, unless it belongs to a package manager.
//!
//! The data directory is the one thing kept by default, because it holds work
//! the user produced rather than anything Refinery installed. Removal is
//! confirmed on a terminal and requires `--yes` anywhere else, so a scripted
//! invocation cannot delete a case history that nobody asked it to.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::credentials::{CredentialKey, CredentialStore};
use crate::config::Config;
use crate::diagnostics::service::ServiceManager;
use crate::error::{AppError, Result};

/// What an uninstall run was asked to do.
#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    /// Also delete the data directory: settings, database, media, and logs.
    pub purge: bool,
    /// Leave the executable in place.
    pub keep_binary: bool,
    /// Proceed without asking for confirmation.
    pub assume_yes: bool,
    /// Binary to remove. Defaults to the running executable.
    pub executable: Option<PathBuf>,
}

/// What `uninstall` removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallReport {
    /// Whether a service unit file was found and deleted.
    pub service_removed: bool,
    /// The unit file that was deleted, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_path: Option<PathBuf>,
    /// Secrets that were deleted, described for a human.
    pub credentials_removed: Vec<String>,
    /// The data directory, when `--purge` deleted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir_removed: Option<PathBuf>,
    /// The executable, when it was deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_removed: Option<PathBuf>,
    /// Anything that was left behind and why, for the closing summary.
    pub remaining: Vec<String>,
}

/// Remove the installation described by `config`.
///
/// The credential store and the service manager are passed in rather than
/// resolved here for the same reason the setup flow takes a console: a test
/// must be able to exercise the whole removal without deleting the login agent
/// or the keychain entries of the machine running it. Confirmation is likewise
/// the caller's job — `run` in [`crate::cli`] asks before calling this.
pub fn uninstall(
    config: &Config,
    credentials: &CredentialStore,
    service: &ServiceManager,
    options: &UninstallOptions,
) -> Result<UninstallReport> {
    let mut report = UninstallReport {
        service_removed: false,
        unit_path: None,
        credentials_removed: Vec::new(),
        data_dir_removed: None,
        binary_removed: None,
        remaining: Vec::new(),
    };

    // A supervisor that still has a unit pointing at a deleted binary respawns
    // it forever, so stopping comes first and its failure is fatal rather than
    // best-effort.
    if service.is_supported() {
        service.stop()?;
        report.service_removed = service.uninstall()?;
        report.unit_path = service.unit_path().map(Path::to_path_buf);
    }

    for key in [
        CredentialKey::provider_api_key(&config.settings.provider.backend),
        CredentialKey::ApiBearerToken,
    ] {
        if credentials.contains(&key)? {
            credentials.delete(&key)?;
            report.credentials_removed.push(key.describe());
        }
    }

    if options.purge {
        let root = config.data_dir.root().to_path_buf();
        match std::fs::remove_dir_all(&root) {
            Ok(()) => report.data_dir_removed = Some(root),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(AppError::io(&root, source)),
        }
    } else {
        report.remaining.push(format!(
            "{} (settings, database, media, logs) — delete it with `--purge`",
            config.data_dir.root().display()
        ));
    }

    if options.keep_binary {
        return Ok(report);
    }

    let executable = match &options.executable {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .map_err(|source| AppError::io("the running executable", source))?,
    };
    // Resolve the link before deleting: a `~/.local/bin` shim and the file it
    // points at are two different things, and deleting only one of them leaves
    // either a dangling command or an orphaned binary.
    let executable =
        std::fs::canonicalize(&executable).map_err(|source| AppError::io(&executable, source))?;
    crate::cli::update::refuse_managed_location(&executable)?;
    match std::fs::remove_file(&executable) {
        Ok(()) => report.binary_removed = Some(executable),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(AppError::io(&executable, source)),
    }

    Ok(report)
}

/// The lines `uninstall` prints when it is not asked for JSON.
pub fn render(report: &UninstallReport) -> String {
    let mut lines = Vec::new();
    match (&report.unit_path, report.service_removed) {
        (Some(path), true) => lines.push(format!(
            "Stopped the service and removed {}.",
            path.display()
        )),
        (Some(_), false) => lines.push("No service was installed.".to_owned()),
        (None, _) => {}
    }
    if report.credentials_removed.is_empty() {
        lines.push("No stored credentials to remove.".to_owned());
    } else {
        for credential in &report.credentials_removed {
            lines.push(format!("Removed the {credential}."));
        }
    }
    if let Some(path) = &report.data_dir_removed {
        lines.push(format!("Deleted the data directory {}.", path.display()));
    }
    if let Some(path) = &report.binary_removed {
        lines.push(format!("Deleted {}.", path.display()));
    } else {
        lines.push("Left the executable in place.".to_owned());
    }
    for remaining in &report.remaining {
        lines.push(format!("Kept {remaining}."));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DataDir;

    /// A configuration rooted in a temporary directory, with the credential
    /// store pinned to the fallback file so no test touches the keychain of the
    /// machine running it.
    fn config_in(root: &Path) -> Config {
        let data_dir = DataDir::at(root).expect("data dir");
        data_dir.ensure().expect("ensure");
        Config {
            data_dir,
            settings: crate::config::Settings::default(),
        }
    }

    fn store(config: &Config) -> CredentialStore {
        CredentialStore::file_backed(&config.data_dir.credentials_dir())
    }

    /// A service manager that writes and reports on a unit file inside the
    /// test's own directory and never invokes launchd or systemd.
    fn service(config: &Config, root: &Path) -> ServiceManager {
        ServiceManager::unsupervised_at(config.data_dir.clone(), root.join("unit"))
            .expect("service manager")
    }

    /// Nothing outside the data directory is touched, and the binary that is
    /// removed is a copy the test owns.
    fn options(executable: &Path) -> UninstallOptions {
        UninstallOptions {
            keep_binary: false,
            assume_yes: true,
            executable: Some(executable.to_path_buf()),
            ..UninstallOptions::default()
        }
    }

    #[test]
    fn an_uninstall_keeps_the_data_directory_unless_it_is_purged() {
        let root = tempfile::tempdir().expect("temp");
        let config = config_in(&root.path().join("data"));
        let binary = root.path().join("refinery");
        std::fs::write(&binary, b"binary").expect("write binary");

        let service = service(&config, root.path());
        service.install().expect("install unit");
        let report =
            uninstall(&config, &store(&config), &service, &options(&binary)).expect("uninstall");

        assert!(report.service_removed);
        assert!(!root.path().join("unit").exists());

        assert!(config.data_dir.root().exists());
        assert_eq!(report.data_dir_removed, None);
        assert!(!binary.exists());
        assert!(report.binary_removed.is_some());
        assert_eq!(report.remaining.len(), 1);
    }

    #[test]
    fn purge_deletes_the_data_directory() {
        let root = tempfile::tempdir().expect("temp");
        let config = config_in(&root.path().join("data"));
        let binary = root.path().join("refinery");
        std::fs::write(&binary, b"binary").expect("write binary");

        let report = uninstall(
            &config,
            &store(&config),
            &service(&config, root.path()),
            &UninstallOptions {
                purge: true,
                ..options(&binary)
            },
        )
        .expect("uninstall");

        assert!(!config.data_dir.root().exists());
        assert_eq!(
            report.data_dir_removed.as_deref(),
            Some(config.data_dir.root())
        );
        assert!(report.remaining.is_empty());
    }

    #[test]
    fn stored_secrets_are_removed_even_without_purge() {
        let root = tempfile::tempdir().expect("temp");
        let config = config_in(&root.path().join("data"));
        let credentials = store(&config);
        credentials
            .set(
                &CredentialKey::ApiBearerToken,
                &crate::domain::SecretString::new("token"),
            )
            .expect("store token");

        let binary = root.path().join("refinery");
        std::fs::write(&binary, b"binary").expect("write binary");
        let report = uninstall(
            &config,
            &credentials,
            &service(&config, root.path()),
            &UninstallOptions {
                keep_binary: true,
                ..options(&binary)
            },
        )
        .expect("uninstall");

        assert!(!credentials
            .contains(&CredentialKey::ApiBearerToken)
            .expect("contains"));
        assert!(report
            .credentials_removed
            .iter()
            .any(|line| line.contains("token")));
        assert!(binary.exists());
        assert_eq!(report.binary_removed, None);
    }
}
