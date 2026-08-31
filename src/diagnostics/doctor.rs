//! `refinery doctor`: what is wrong, and what to do about it.
//!
//! The operational model lists eight things that can be broken. Each is an
//! independent check, and independence is the point: a missing API key must not
//! stop the database check from running, because the user needs the whole list
//! in one pass rather than one problem per invocation.
//!
//! Every check returns a status and a remedy. A finding without a remedy is a
//! complaint, not a diagnosis, so [`Finding::remedy`] is required on anything
//! that is not a pass — it is the line the user actually acts on.
//!
//! The distinction between `warn` and `fail` is whether refinement can happen
//! right now. A credential kept in the fallback file is a warning: it works,
//! and the user should know the trade-off. A missing API key is a failure: no
//! case can complete until it is set.

use std::fmt;

use crate::agent;
use crate::config::credentials::{CredentialKey, CredentialStore};
use crate::config::Config;
use crate::diagnostics::redaction;
use crate::diagnostics::service::{ServiceManager, ServiceStatus};
use crate::storage::Storage;

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Working as intended.
    Pass,
    /// Working, but degraded or not what the product recommends.
    Warn,
    /// Not working. Refinement cannot complete until it is fixed.
    Fail,
}

impl Status {
    /// The fixed-width label used in the report.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What was checked, for example `credential store`.
    pub check: &'static str,
    /// Whether it is working.
    pub status: Status,
    /// What was observed.
    pub detail: String,
    /// What the user should do about it. Always present unless the check
    /// passed, because a finding the user cannot act on wastes their time.
    pub remedy: Option<String>,
}

impl Finding {
    /// A passing finding.
    pub fn pass(check: &'static str, detail: impl Into<String>) -> Self {
        Self {
            check,
            status: Status::Pass,
            detail: detail.into(),
            remedy: None,
        }
    }

    /// A warning with its remedy.
    pub fn warn(check: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            check,
            status: Status::Warn,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }

    /// A failure with its remedy.
    pub fn fail(check: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            check,
            status: Status::Fail,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// Every finding from one run of `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The findings, in the order the checks ran.
    pub findings: Vec<Finding>,
}

impl Report {
    /// The most severe status in the report.
    pub fn worst(&self) -> Status {
        self.findings
            .iter()
            .map(|finding| finding.status)
            .max()
            .unwrap_or(Status::Pass)
    }

    /// Whether anything failed.
    pub fn has_failures(&self) -> bool {
        self.worst() == Status::Fail
    }

    /// How many findings carry each status.
    pub fn count(&self, status: Status) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.status == status)
            .count()
    }

    /// Render the report for a terminal.
    ///
    /// Every line is passed through the redaction layer on the way out: the
    /// provider check quotes provider errors, and a provider is entitled to
    /// echo the offending key back at us.
    pub fn render(&self) -> String {
        let width = self
            .findings
            .iter()
            .map(|finding| finding.check.len())
            .max()
            .unwrap_or(0);

        let mut out = String::new();
        for finding in &self.findings {
            out.push_str(&format!(
                "{:<4}  {:<width$}  {}\n",
                finding.status.label(),
                finding.check,
                finding.detail,
                width = width
            ));
            if let Some(remedy) = &finding.remedy {
                out.push_str(&format!(
                    "{:<4}  {:<width$}  → {remedy}\n",
                    "",
                    "",
                    width = width
                ));
            }
        }
        out.push_str(&format!(
            "\n{} passed, {} warned, {} failed\n",
            self.count(Status::Pass),
            self.count(Status::Warn),
            self.count(Status::Fail),
        ));
        redaction::redact(&out).into_owned()
    }
}

/// Run every check.
///
/// Checks that need the network are skipped when `offline` is set, so `doctor`
/// stays useful on a machine with no connectivity rather than reporting a
/// provider failure that is really a missing network.
pub async fn run(config: &Config, offline: bool) -> Report {
    let credentials = CredentialStore::open(&config.data_dir.credentials_dir());
    let service = ServiceManager::new(config.data_dir.clone());
    run_with(config, &credentials, service.as_ref().ok(), offline).await
}

/// Run every check against a caller-supplied credential store and service
/// manager.
///
/// Setup calls this with the very store it just wrote to. Opening a second
/// store here would let setup save a key to one place and the closing health
/// check look for it in another — which is exactly the false failure this
/// signature exists to prevent.
pub async fn run_with(
    config: &Config,
    credentials: &CredentialStore,
    service: Option<&ServiceManager>,
    offline: bool,
) -> Report {
    let mut findings = vec![check_data_directory(config)];
    let storage = Storage::open(config.data_dir.database_file()).await;
    findings.extend(check_database(&storage).await);
    findings.push(check_credential_store(credentials));

    let api_key = credentials.get(&CredentialKey::provider_api_key(
        &config.settings.provider.backend,
    ));
    findings.extend(check_provider(config, &api_key, offline).await);
    findings.extend(check_repositories(&storage).await);
    findings.push(check_local_api(credentials));
    findings.push(check_overlord(config));
    findings.push(check_service(config, service));

    Report { findings }
}

const DATA_DIRECTORY: &str = "data directory";
const DATABASE: &str = "database";
const CREDENTIAL_STORE: &str = "credential store";
const PROVIDER: &str = "provider";
const REPOSITORIES: &str = "repositories";
const LOCAL_API: &str = "local API";
const OVERLORD: &str = "Overlord";
const SERVICE: &str = "service";

/// Configuration and data-directory permissions.
///
/// Refinery tightens the directories it owns on every start, so a loosened
/// directory usually repairs itself before this check runs. Files are the
/// unrepaired case and the one that matters: the credential fallback file, the
/// settings file, and the database are created owner-only and never rewritten,
/// so a `chmod` or a restore from an archive can leave a secret readable by
/// everyone on the machine with nothing to notice it.
fn check_data_directory(config: &Config) -> Finding {
    let root = config.data_dir.root();
    if !root.is_dir() {
        return Finding::fail(
            DATA_DIRECTORY,
            format!("{} does not exist", root.display()),
            "run `refinery setup`, which creates it",
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut exposed = Vec::new();
        let mut unreadable = None;

        let credentials = config
            .data_dir
            .credentials_dir()
            .join(crate::config::credentials::FALLBACK_FILE);
        let candidates = [
            root.to_path_buf(),
            config.data_dir.credentials_dir(),
            credentials,
            config.data_dir.settings_file(),
            config.data_dir.database_file(),
        ];

        for path in candidates {
            if !path.exists() {
                continue;
            }
            match std::fs::metadata(&path) {
                // Group or world access exposes case media and, where the
                // credential fallback is in use, the API key itself.
                Ok(metadata) if metadata.permissions().mode() & 0o077 != 0 => {
                    exposed.push((path, metadata.permissions().mode() & 0o777));
                }
                Ok(_) => {}
                Err(source) => unreadable = Some((path, source)),
            }
        }

        if let Some((path, source)) = unreadable {
            return Finding::fail(
                DATA_DIRECTORY,
                format!("{} cannot be inspected: {source}", path.display()),
                "check that the data directory is owned by this user",
            );
        }

        if !exposed.is_empty() {
            let detail = exposed
                .iter()
                .map(|(path, mode)| format!("{} (mode {mode:04o})", path.display()))
                .collect::<Vec<_>>()
                .join(", ");
            let remedy = exposed
                .iter()
                .map(|(path, _)| {
                    format!(
                        "chmod {} {}",
                        if path.is_dir() { "700" } else { "600" },
                        path.display()
                    )
                })
                .collect::<Vec<_>>()
                .join(" && ");
            return Finding::fail(
                DATA_DIRECTORY,
                format!("readable beyond its owner: {detail}"),
                format!("run `{remedy}`"),
            );
        }
    }

    Finding::pass(
        DATA_DIRECTORY,
        format!("{} is present and owner-only", root.display()),
    )
}

/// Migrations and integrity.
async fn check_database(storage: &crate::error::Result<Storage>) -> Vec<Finding> {
    let storage = match storage {
        Ok(storage) => storage,
        Err(error) => {
            return vec![Finding::fail(
                DATABASE,
                redaction::redact_detail(error),
                "check that the data directory is writable and not on a network filesystem",
            )]
        }
    };

    let mut findings = Vec::new();
    match storage.pending_migrations().await {
        Ok(pending) if pending.is_empty() => {}
        Ok(pending) => findings.push(Finding::fail(
            DATABASE,
            format!("{} migration(s) have not been applied", pending.len()),
            "run any refinery command with this data directory to apply them, or upgrade Refinery",
        )),
        Err(error) => findings.push(Finding::fail(
            DATABASE,
            redaction::redact_detail(&error),
            "the database may be from a newer Refinery; upgrade Refinery",
        )),
    }

    match storage.integrity_problems().await {
        Ok(problems) if problems.is_empty() => {
            let cases = storage.case_counts_by_state().await.unwrap_or_default();
            let total: u64 = cases.values().sum();
            findings.push(Finding::pass(
                DATABASE,
                format!("migrations applied, integrity check clean, {total} case(s)"),
            ));
        }
        Ok(problems) => findings.push(Finding::fail(
            DATABASE,
            format!(
                "integrity check reported {} problem(s): {}",
                problems.len(),
                problems.join("; ")
            ),
            "restore the database from a backup; a corrupted file cannot be repaired in place",
        )),
        Err(error) => findings.push(Finding::fail(
            DATABASE,
            redaction::redact_detail(&error),
            "check that the data directory is writable",
        )),
    }

    findings
}

/// Whether secrets have a proper home.
fn check_credential_store(credentials: &CredentialStore) -> Finding {
    if credentials.backend().is_fallback() {
        return Finding::warn(
            CREDENTIAL_STORE,
            format!(
                "no operating-system credential store is available ({}); secrets are in {}",
                credentials
                    .unavailable_reason()
                    .unwrap_or("reason not reported"),
                credentials.fallback_path().display()
            ),
            "this works, but the key is a file rather than a managed secret: exclude the data directory from backups, or install a Secret Service provider such as gnome-keyring",
        );
    }
    Finding::pass(CREDENTIAL_STORE, credentials.backend().label())
}

/// Whether the configured provider accepts the stored credential.
async fn check_provider(
    config: &Config,
    api_key: &crate::error::Result<Option<crate::domain::secret::SecretString>>,
    offline: bool,
) -> Vec<Finding> {
    let backend = &config.settings.provider.backend;
    let model = &config.settings.provider.model;

    let key = match api_key {
        Ok(Some(key)) => key,
        Ok(None) => {
            return vec![Finding::fail(
                PROVIDER,
                format!("no {backend} API key is stored"),
                format!("run `refinery provider configure {backend}`"),
            )]
        }
        Err(error) => {
            return vec![Finding::fail(
                PROVIDER,
                redaction::redact_detail(error),
                format!("run `refinery provider configure {backend}` to store the key again"),
            )]
        }
    };

    if backend != "gemini" {
        return vec![Finding::warn(
            PROVIDER,
            format!("a {backend} key is stored, but this build can only test gemini"),
            "set provider.backend to `gemini` in refinery.toml, or upgrade Refinery",
        )];
    }

    if offline {
        return vec![Finding::warn(
            PROVIDER,
            format!("a {backend} key is stored; the connection was not tested"),
            "run `refinery doctor` without `--offline` to verify the credential",
        )];
    }

    match agent::test_gemini(agent::GEMINI_API_BASE, model, key).await {
        Ok(connection) => vec![Finding::pass(
            PROVIDER,
            format!("{backend} authenticated: {}", connection.summary()),
        )],
        Err(error) => {
            let remedy = match error {
                crate::error::AppError::NeedsUser { .. } => {
                    format!("the stored key was rejected; run `refinery provider configure {backend}` with a valid key")
                }
                crate::error::AppError::Upstream { retry, .. } if retry.is_retryable() => {
                    "the provider or the network is temporarily unavailable; try again shortly"
                        .to_owned()
                }
                _ => format!(
                    "check that provider.model in refinery.toml names a model this key can use (currently `{model}`)"
                ),
            };
            vec![Finding::fail(
                PROVIDER,
                redaction::redact_detail(&error),
                remedy,
            )]
        }
    }
}

/// Whether registered repositories are still where they were registered.
async fn check_repositories(storage: &crate::error::Result<Storage>) -> Vec<Finding> {
    let Ok(storage) = storage else {
        // The database finding already explains this; repeating it adds noise.
        return Vec::new();
    };

    let repositories = match storage.list_repositories().await {
        Ok(repositories) => repositories,
        Err(error) => {
            return vec![Finding::fail(
                REPOSITORIES,
                redaction::redact_detail(&error),
                "the repository registrations could not be read; check the database finding above",
            )]
        }
    };

    if repositories.is_empty() {
        return vec![Finding::warn(
            REPOSITORIES,
            "no repositories are registered",
            "run `refinery repository add .` in a project so refinements can be grounded in code",
        )];
    }

    let unreachable: Vec<_> = repositories
        .iter()
        .filter(|repository| !repository.is_reachable())
        .collect();

    if unreachable.is_empty() {
        return vec![Finding::pass(
            REPOSITORIES,
            format!("{} registered, all reachable", repositories.len()),
        )];
    }

    vec![Finding::fail(
        REPOSITORIES,
        format!(
            "{} of {} registered repositories are missing: {}",
            unreachable.len(),
            repositories.len(),
            unreachable
                .iter()
                .map(|repository| repository.root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "the directories were moved or deleted; re-register them with `refinery repository add <path>`",
    )]
}

/// Whether local clients can authenticate to the loopback API.
fn check_local_api(credentials: &CredentialStore) -> Finding {
    match credentials.contains(&CredentialKey::ApiBearerToken) {
        Ok(true) => Finding::pass(LOCAL_API, "a local API token is stored"),
        Ok(false) => Finding::fail(
            LOCAL_API,
            "no local API token is stored, so no client can authenticate",
            "run `refinery setup`, which generates one",
        ),
        Err(error) => Finding::fail(
            LOCAL_API,
            redaction::redact_detail(&error),
            "run `refinery setup` to generate a new local API token",
        ),
    }
}

/// Whether an Overlord destination is configured.
///
/// The full round-trip check needs the delivery adapter from M7. What can be
/// established now — and what most often goes wrong — is whether a destination
/// is configured at all.
fn check_overlord(config: &Config) -> Finding {
    match &config.settings.overlord.base_url {
        Some(url) => Finding::pass(OVERLORD, format!("destination configured: {url}")),
        None => Finding::warn(
            OVERLORD,
            "no Overlord destination is configured",
            "run `refinery setup` to point Refinery at a local Overlord, or deliver results by local export",
        ),
    }
}

/// Whether the background service is installed and running.
fn check_service(config: &Config, manager: Option<&ServiceManager>) -> Finding {
    let Some(manager) = manager else {
        return Finding::warn(
            SERVICE,
            "the background service could not be inspected",
            "run `refinery serve` to run the daemon in the foreground",
        );
    };

    match manager.status() {
        ServiceStatus::Running { pid } => Finding::pass(
            SERVICE,
            match pid {
                Some(pid) => format!("running (pid {pid}), listening on {}", config.settings.api.bind_address()),
                None => format!("running, listening on {}", config.settings.api.bind_address()),
            },
        ),
        ServiceStatus::Installed => Finding::fail(
            SERVICE,
            "the service is installed but not running, so submissions will not be accepted",
            "run `refinery service start`",
        ),
        ServiceStatus::NotInstalled => Finding::warn(
            SERVICE,
            "the background service is not installed",
            "run `refinery service start` to install and start it, or `refinery serve` to run it in the foreground",
        ),
        ServiceStatus::Unsupported { platform } => Finding::warn(
            SERVICE,
            format!("no supported service manager on {platform}"),
            "run `refinery serve` to run the daemon in the foreground",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DataDir;
    use crate::domain::secret::SecretString;

    fn config(temp: &tempfile::TempDir) -> Config {
        Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve")).expect("load")
    }

    /// Run the checks against a store and service manager that exist only for
    /// this test.
    ///
    /// A test must never read the credential store or the login session of the
    /// machine running it: doing so makes the suite depend on the developer's
    /// keychain, and on macOS it can block on an authorization prompt that
    /// nobody is there to answer.
    async fn report_for(config: &Config) -> Report {
        let credentials = CredentialStore::file_backed(&config.data_dir.credentials_dir());
        let service = ServiceManager::unsupervised_at(
            config.data_dir.clone(),
            config.data_dir.run_dir().join("service.unit"),
        )
        .expect("service manager");
        run_with(config, &credentials, Some(&service), true).await
    }

    #[test]
    fn the_worst_status_drives_the_report() {
        let report = Report {
            findings: vec![
                Finding::pass("a", "fine"),
                Finding::warn("b", "degraded", "do something"),
            ],
        };
        assert_eq!(report.worst(), Status::Warn);
        assert!(!report.has_failures());

        let report = Report {
            findings: vec![Finding::fail("c", "broken", "fix it")],
        };
        assert!(report.has_failures());
    }

    #[test]
    fn every_non_passing_finding_carries_a_remedy() {
        // A finding without a remedy tells the user they have a problem and
        // nothing more, which is the failure mode this check exists to prevent.
        let report = Report {
            findings: vec![
                Finding::pass("a", "fine"),
                Finding::warn("b", "degraded", "do this"),
                Finding::fail("c", "broken", "do that"),
            ],
        };
        for finding in &report.findings {
            assert_eq!(
                finding.remedy.is_some(),
                finding.status != Status::Pass,
                "{} has the wrong remedy shape",
                finding.check
            );
        }
    }

    #[test]
    fn the_rendered_report_shows_remedies_and_a_summary() {
        let report = Report {
            findings: vec![Finding::fail("provider", "no key stored", "run setup")],
        };
        let text = report.render();
        assert!(text.contains("fail"));
        assert!(text.contains("no key stored"));
        assert!(text.contains("→ run setup"));
        assert!(text.contains("0 passed, 0 warned, 1 failed"));
    }

    #[test]
    fn a_rendered_report_never_shows_a_credential() {
        let _guard = redaction::test_support::registry_lock();
        redaction::clear_registered_secrets();
        redaction::register_secret("AIzaTestSecretValue00000");
        let report = Report {
            findings: vec![Finding::fail(
                "provider",
                "rejected key AIzaTestSecretValue00000",
                "store a valid key",
            )],
        };
        let text = report.render();
        redaction::clear_registered_secrets();
        assert!(!text.contains("AIzaTestSecretValue00000"));
    }

    #[tokio::test]
    async fn a_fresh_installation_fails_on_the_missing_key_and_token() {
        let temp = tempfile::tempdir().expect("temp dir");
        let report = report_for(&config(&temp)).await;

        let provider = finding(&report, PROVIDER);
        assert_eq!(provider.status, Status::Fail);
        assert!(provider.detail.contains("no gemini API key"));
        assert!(provider
            .remedy
            .as_deref()
            .expect("remedy")
            .contains("refinery provider configure gemini"));

        assert_eq!(finding(&report, LOCAL_API).status, Status::Fail);
        assert_eq!(finding(&report, DATA_DIRECTORY).status, Status::Pass);
        assert_eq!(finding(&report, DATABASE).status, Status::Pass);
    }

    #[tokio::test]
    async fn an_unregistered_repository_is_a_warning_not_a_failure() {
        let temp = tempfile::tempdir().expect("temp dir");
        let report = report_for(&config(&temp)).await;
        let repositories = finding(&report, REPOSITORIES);
        assert_eq!(repositories.status, Status::Warn);
        assert!(repositories
            .remedy
            .as_deref()
            .expect("remedy")
            .contains("repository add"));
    }

    #[tokio::test]
    async fn a_repository_that_moved_is_detected_and_explained() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = config(&temp);
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("create");

        let storage = Storage::open(config.data_dir.database_file())
            .await
            .expect("open");
        storage
            .register_repository(
                &std::fs::canonicalize(&project).expect("canonical"),
                &crate::repositories::RepositoryPolicy::default(),
                chrono::Utc::now(),
            )
            .await
            .expect("register");
        drop(storage);
        std::fs::remove_dir_all(&project).expect("remove");

        let report = report_for(&config).await;
        let repositories = finding(&report, REPOSITORIES);
        assert_eq!(repositories.status, Status::Fail);
        assert!(repositories.detail.contains("project"));
        assert!(repositories
            .remedy
            .as_deref()
            .expect("remedy")
            .contains("repository add"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_loosened_data_directory_is_detected_with_a_chmod_remedy() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let config = config(&temp);
        std::fs::set_permissions(
            config.data_dir.root(),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("loosen");

        let report = report_for(&config).await;
        let finding = finding(&report, DATA_DIRECTORY);
        assert_eq!(finding.status, Status::Fail);
        assert!(finding.detail.contains("readable beyond its owner"));
        assert!(finding
            .remedy
            .as_deref()
            .expect("remedy")
            .contains("chmod 700"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_world_readable_credential_file_is_detected_with_a_chmod_remedy() {
        use std::os::unix::fs::PermissionsExt;

        // Unlike a directory, this is never repaired on start: a restore from
        // an archive or a stray `chmod -R` leaves the API key readable by every
        // account on the machine, and nothing else would notice.
        let temp = tempfile::tempdir().expect("temp dir");
        let config = config(&temp);
        let store = CredentialStore::file_backed(&config.data_dir.credentials_dir());
        store
            .set(
                &CredentialKey::provider_api_key("gemini"),
                &SecretString::new("AIzaExposedKeyValue00000"),
            )
            .expect("store");
        std::fs::set_permissions(
            store.fallback_path(),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("loosen");

        let report = report_for(&config).await;
        let finding = finding(&report, DATA_DIRECTORY);
        assert_eq!(finding.status, Status::Fail);
        assert!(
            finding.detail.contains("credentials.json"),
            "{}",
            finding.detail
        );
        assert!(finding
            .remedy
            .as_deref()
            .expect("remedy")
            .contains("chmod 600"));
    }

    #[tokio::test]
    async fn a_stored_token_and_key_turn_their_checks_green() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = config(&temp);
        let store = CredentialStore::file_backed(&config.data_dir.credentials_dir());
        store
            .set(
                &CredentialKey::ApiBearerToken,
                &SecretString::new("rfy_doctor_test_token_value"),
            )
            .expect("store token");

        assert_eq!(check_local_api(&store).status, Status::Pass);
        // The fallback file is a working configuration that the user should
        // nonetheless be told about.
        assert_eq!(check_credential_store(&store).status, Status::Warn);
    }

    #[tokio::test]
    async fn an_offline_run_reports_the_untested_credential_rather_than_a_false_failure() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = config(&temp);
        let key = Ok(Some(SecretString::new("AIzaOfflineTestKeyValue0")));

        let findings = check_provider(&config, &key, true).await;
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].status, Status::Warn);
        assert!(findings[0].detail.contains("not tested"));
    }

    #[tokio::test]
    async fn an_unknown_backend_is_reported_rather_than_tested() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut config = config(&temp);
        config.settings.provider.backend = "something-else".into();
        let key = Ok(Some(SecretString::new("some-stored-key-value")));

        let findings = check_provider(&config, &key, false).await;
        assert_eq!(findings[0].status, Status::Warn);
        assert!(findings[0].detail.contains("can only test gemini"));
    }

    #[tokio::test]
    async fn every_operational_check_appears_in_the_report() {
        let temp = tempfile::tempdir().expect("temp dir");
        let report = report_for(&config(&temp)).await;
        for check in [
            DATA_DIRECTORY,
            DATABASE,
            CREDENTIAL_STORE,
            PROVIDER,
            REPOSITORIES,
            LOCAL_API,
            OVERLORD,
            SERVICE,
        ] {
            assert!(
                report.findings.iter().any(|finding| finding.check == check),
                "the operational model requires a `{check}` check"
            );
        }
    }

    fn finding<'a>(report: &'a Report, check: &str) -> &'a Finding {
        report
            .findings
            .iter()
            .find(|finding| finding.check == check)
            .unwrap_or_else(|| panic!("no `{check}` finding"))
    }
}
