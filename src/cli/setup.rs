//! `refinery setup`: the guided first run.
//!
//! The product's promise is that installation and setup complete without
//! editing a file. That is the constraint this module is written against:
//! every value setup needs is either asked for, detected, or defaulted, and
//! anything the user declines leaves a working installation with a `doctor`
//! finding explaining what is still missing.
//!
//! The seven steps come from the product description:
//!
//! 1. explain what stays local and what is sent to the provider;
//! 2. take a Gemini API key and store it in the credential store;
//! 3. test the provider connection without displaying the secret;
//! 4. offer to register the current directory as a repository;
//! 5. detect or configure the local Overlord connection;
//! 6. install and start the background service if requested; and
//! 7. run a final health check and show how to reopen configuration.
//!
//! Two rules shape the implementation. Setup is resumable: it reads what is
//! already configured and offers to keep it, so a user who quits halfway and
//! runs it again is not starting over. And no step blocks another: declining
//! the service or the repository does not prevent the key from being stored.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use crate::agent;
use crate::config::credentials::{CredentialBackend, CredentialKey, CredentialStore};
use crate::config::settings::validate_overlord_url;
use crate::config::Config;
use crate::diagnostics::doctor;
use crate::diagnostics::service::ServiceManager;
use crate::domain::secret::SecretString;
use crate::error::{AppError, Result};
use crate::repositories::{canonical_root, RepositoryPolicy};
use crate::storage::Storage;

/// Where the prompts are read from and written to.
///
/// Threading this through rather than reaching for `stdin` directly is what
/// lets the whole flow be tested: a scripted answer sequence runs the real
/// setup, not a reimplementation of it.
pub struct Console<'a> {
    input: &'a mut dyn BufRead,
    output: &'a mut dyn Write,
    interactive: bool,
}

impl<'a> Console<'a> {
    /// A console over explicit streams.
    pub fn new(input: &'a mut dyn BufRead, output: &'a mut dyn Write, interactive: bool) -> Self {
        Self {
            input,
            output,
            interactive,
        }
    }

    fn say(&mut self, line: impl AsRef<str>) {
        let _ = writeln!(self.output, "{}", line.as_ref());
    }

    fn blank(&mut self) {
        let _ = writeln!(self.output);
    }

    /// Read one line, returning `None` at end of input.
    fn read_line(&mut self) -> Option<String> {
        let mut buffer = String::new();
        match self.input.read_line(&mut buffer) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(buffer.trim_end_matches(['\n', '\r']).to_owned()),
        }
    }

    /// Ask a free-text question with an optional default.
    fn ask(&mut self, question: &str, default: Option<&str>) -> Option<String> {
        match default {
            Some(default) => {
                let _ = write!(self.output, "{question} [{default}]: ");
            }
            None => {
                let _ = write!(self.output, "{question}: ");
            }
        }
        let _ = self.output.flush();

        let answer = self.read_line()?;
        let answer = answer.trim();
        if answer.is_empty() {
            return default.map(str::to_owned);
        }
        Some(answer.to_owned())
    }

    /// Ask a yes/no question. End of input takes the default, so a
    /// non-interactive run completes rather than hanging or erroring.
    fn confirm(&mut self, question: &str, default: bool) -> bool {
        let hint = if default { "Y/n" } else { "y/N" };
        let _ = write!(self.output, "{question} [{hint}]: ");
        let _ = self.output.flush();

        match self.read_line() {
            None => {
                let _ = writeln!(self.output);
                default
            }
            Some(answer) => match answer.trim().to_ascii_lowercase().as_str() {
                "" => default,
                "y" | "yes" => true,
                "n" | "no" => false,
                _ => default,
            },
        }
    }

    /// Ask for a secret.
    ///
    /// On a real terminal the input is read without echo, so the key does not
    /// stay on screen or in the scrollback. Where the input is not a terminal —
    /// a test, or a piped script — it is read as an ordinary line, because
    /// there is no echo to suppress.
    fn ask_secret(&mut self, question: &str) -> Option<SecretString> {
        let _ = write!(self.output, "{question}: ");
        let _ = self.output.flush();

        let raw = if self.interactive {
            let value = read_without_echo(self.input);
            let _ = writeln!(self.output);
            value?
        } else {
            self.read_line()?
        };

        let raw = raw.trim();
        if raw.is_empty() {
            None
        } else {
            Some(SecretString::new(raw))
        }
    }
}

/// Read a line with terminal echo disabled where the platform allows it.
///
/// Falling back to an echoing read is deliberate: refusing to accept a key on a
/// terminal Refinery cannot put into raw mode would block setup entirely, which
/// is a worse outcome than a key briefly visible on the user's own screen.
fn read_without_echo(input: &mut dyn BufRead) -> Option<String> {
    #[cfg(unix)]
    {
        if let Some(value) = unix_read_without_echo(input) {
            return Some(value);
        }
    }
    let mut buffer = String::new();
    match input.read_line(&mut buffer) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buffer.trim_end_matches(['\n', '\r']).to_owned()),
    }
}

#[cfg(unix)]
fn unix_read_without_echo(input: &mut dyn BufRead) -> Option<String> {
    // `stty` is used rather than a `termios` binding so the crate keeps its
    // `unsafe_code = "forbid"` lint for a single terminal-mode toggle.
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let disabled = std::process::Command::new("stty")
        .args(["-echo"])
        .stdin(std::process::Stdio::inherit())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !disabled {
        return None;
    }

    let mut buffer = String::new();
    let read = input.read_line(&mut buffer);

    let _ = std::process::Command::new("stty")
        .args(["echo"])
        .stdin(std::process::Stdio::inherit())
        .status();

    match read {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buffer.trim_end_matches(['\n', '\r']).to_owned()),
    }
}

/// What setup did, so the caller can report an exit status and tests can
/// assert on the outcome rather than on printed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOutcome {
    /// Whether a provider API key is now stored.
    pub provider_key_stored: bool,
    /// Whether the provider connection was tested and succeeded.
    pub provider_verified: bool,
    /// The repository registered during this run, if any.
    pub repository_registered: Option<PathBuf>,
    /// The Overlord base URL configured during this run, if any.
    pub overlord_configured: Option<String>,
    /// Whether the background service was started.
    pub service_started: bool,
    /// Whether the closing health check found failures.
    pub health_check_failed: bool,
}

/// Options that change how setup runs, without changing what it asks.
#[derive(Debug, Clone, Default)]
pub struct SetupOptions {
    /// Skip every network call. Used by tests and by a first run on a machine
    /// that is not yet online.
    pub offline: bool,
    /// Keep secrets in the fallback file rather than the OS credential store.
    /// Tests set this so a run never writes to the developer's Keychain.
    pub force_file_credentials: bool,
    /// The directory offered as a repository. Defaults to the working
    /// directory.
    pub working_directory: Option<PathBuf>,
    /// Write the service unit here and never invoke the platform supervisor.
    /// Tests set this so a setup run cannot install a real login agent.
    pub unsupervised_service_unit: Option<PathBuf>,
}

/// Run the guided setup.
pub async fn run(
    config: &mut Config,
    console: &mut Console<'_>,
    options: &SetupOptions,
) -> Result<SetupOutcome> {
    let credentials_dir = config.data_dir.credentials_dir();
    let credentials = if options.force_file_credentials {
        CredentialStore::file_backed(&credentials_dir)
    } else {
        CredentialStore::open(&credentials_dir)
    };

    let mut outcome = SetupOutcome {
        provider_key_stored: false,
        provider_verified: false,
        repository_registered: None,
        overlord_configured: None,
        service_started: false,
        health_check_failed: false,
    };

    let service = match options.unsupervised_service_unit.clone() {
        Some(unit) => ServiceManager::unsupervised_at(config.data_dir.clone(), unit),
        None => ServiceManager::new(config.data_dir.clone()),
    };

    step_one_disclosure(config, console, &credentials);
    step_two_provider_key(config, console, &credentials, &mut outcome)?;
    step_three_connection_test(config, console, &credentials, options, &mut outcome).await;
    step_four_repository(config, console, options, &mut outcome).await?;
    step_five_overlord(config, console, &mut outcome)?;
    step_six_service(console, service.as_ref(), &mut outcome);
    ensure_api_token(console, &credentials)?;
    step_seven_health_check(
        config,
        console,
        &credentials,
        service.as_ref().ok(),
        options,
        &mut outcome,
    )
    .await;

    Ok(outcome)
}

/// Step 1 — say what stays here and what leaves.
fn step_one_disclosure(config: &Config, console: &mut Console<'_>, credentials: &CredentialStore) {
    console.say("Refinery turns transcripts and repository context into validated prompts.");
    console.blank();
    console.say("What stays on this machine:");
    console.say(format!(
        "  cases, attachments, logs, and settings, under {}",
        config.data_dir.root().display()
    ));
    console.say("  repository contents, except the parts the model asks to read");
    console.say(format!(
        "  credentials, in the {}",
        credentials.backend().label()
    ));
    console.blank();
    console.say("What is sent to the model provider:");
    console.say("  the transcript you submit, and any images or video attached to it");
    console.say("  the repository excerpts the model requests while refining");
    console.say("  your questions' answers, and the prompt it produces");
    console.blank();
    console
        .say("Refinery listens on loopback only and never accepts connections from the network.");

    if credentials.backend() == CredentialBackend::FallbackFile {
        console.blank();
        console.say(format!(
            "Note: this system has no credential store ({}).",
            credentials
                .unavailable_reason()
                .unwrap_or("reason not reported")
        ));
        console.say(format!(
            "      Your API key will be kept in {}, readable only by you.",
            credentials.fallback_path().display()
        ));
        console.say("      Exclude the data directory from backups, or install a keyring.");
    }
    console.blank();
}

/// Step 2 — take and store the provider key.
fn step_two_provider_key(
    config: &Config,
    console: &mut Console<'_>,
    credentials: &CredentialStore,
    outcome: &mut SetupOutcome,
) -> Result<()> {
    let backend = config.settings.provider.backend.clone();
    let key = CredentialKey::provider_api_key(&backend);
    let already_stored = credentials.contains(&key).unwrap_or(false);

    if already_stored {
        console.say(format!("A {backend} API key is already stored."));
        outcome.provider_key_stored = true;
        if !console.confirm("Replace it?", false) {
            console.blank();
            return Ok(());
        }
    }

    console.say(format!(
        "Paste your {backend} API key. It is stored in the {} and never displayed.",
        credentials.backend().label()
    ));

    match console.ask_secret(&format!("{backend} API key")) {
        Some(secret) => {
            credentials.set(&key, &secret)?;
            outcome.provider_key_stored = true;
            console.say("Stored.");
        }
        None if already_stored => console.say("Keeping the existing key."),
        None => {
            outcome.provider_key_stored = false;
            console.say(format!(
                "No key entered. Store one later with `refinery provider configure {backend}`."
            ));
        }
    }
    console.blank();
    Ok(())
}

/// Step 3 — prove the key works, without showing it.
async fn step_three_connection_test(
    config: &Config,
    console: &mut Console<'_>,
    credentials: &CredentialStore,
    options: &SetupOptions,
    outcome: &mut SetupOutcome,
) {
    if !outcome.provider_key_stored {
        return;
    }
    if options.offline {
        console.say("Skipping the provider connection test (offline).");
        console.blank();
        return;
    }

    let backend = &config.settings.provider.backend;
    let model = &config.settings.provider.model;
    console.say(format!("Testing the {backend} connection…"));

    let key = match credentials.get(&CredentialKey::provider_api_key(backend)) {
        Ok(Some(key)) => key,
        Ok(None) => {
            console.say("The key could not be read back after being stored.");
            console.blank();
            return;
        }
        Err(error) => {
            console.say(format!(
                "  {}",
                crate::diagnostics::redaction::redact_error(&error)
            ));
            console.blank();
            return;
        }
    };

    match agent::test_gemini(agent::GEMINI_API_BASE, model, &key).await {
        Ok(connection) => {
            outcome.provider_verified = true;
            console.say(format!("  Connected to {}.", connection.summary()));
        }
        Err(error) => {
            // A failed test does not abort setup: the remaining steps still
            // leave the installation better configured than it was.
            console.say(format!(
                "  Could not use the key: {}",
                crate::diagnostics::redaction::redact_error(&error)
            ));
            console.say(format!(
                "  Setup will continue. Fix it later with `refinery provider configure {backend}`."
            ));
        }
    }
    console.blank();
}

/// Step 4 — offer the current directory as a repository.
async fn step_four_repository(
    config: &Config,
    console: &mut Console<'_>,
    options: &SetupOptions,
    outcome: &mut SetupOutcome,
) -> Result<()> {
    let candidate = match options.working_directory.clone() {
        Some(path) => path,
        None => match std::env::current_dir() {
            Ok(path) => path,
            Err(_) => return Ok(()),
        },
    };

    let Ok(root) = canonical_root(&candidate) else {
        // A working directory that cannot be registered — the home directory,
        // say — is not worth explaining here; `repository add` says why.
        return Ok(());
    };

    console.say("Registering a repository lets refinements be grounded in real code.");
    console.say("Access is read-only and bounded by the limits in your settings.");
    if !console.confirm(&format!("Register {}?", root.display()), true) {
        console.say("Skipped. Add one later with `refinery repository add <path>`.");
        console.blank();
        return Ok(());
    }

    let storage = Storage::open(config.data_dir.database_file()).await?;
    let policy = RepositoryPolicy::from_limits(&config.settings.limits);
    let (repository, created) = storage
        .register_repository(&root, &policy, chrono::Utc::now())
        .await?;

    console.say(if created {
        format!("Registered {}.", repository.root.display())
    } else {
        format!(
            "{} was already registered; its policy was refreshed.",
            repository.root.display()
        )
    });
    if !repository.is_git_work_tree() {
        console.say(
            "  Note: this is not a Git working tree, so git_status and git_diff are unavailable.",
        );
    }
    outcome.repository_registered = Some(repository.root);
    console.blank();
    Ok(())
}

/// Step 5 — find or configure the local Overlord.
fn step_five_overlord(
    config: &mut Config,
    console: &mut Console<'_>,
    outcome: &mut SetupOutcome,
) -> Result<()> {
    console.say("Refinery can deliver finished prompts to a local Overlord.");

    let existing = config.settings.overlord.base_url.clone();
    if let Some(existing) = &existing {
        console.say(format!("Currently configured: {existing}"));
        if !console.confirm("Change it?", false) {
            outcome.overlord_configured = Some(existing.clone());
            console.blank();
            return Ok(());
        }
    }

    if !console.confirm("Configure an Overlord destination now?", existing.is_some()) {
        console.say("Skipped. Results can still be exported locally.");
        console.blank();
        return Ok(());
    }

    let default = existing.unwrap_or_else(|| "http://127.0.0.1:3000".to_owned());
    let Some(answer) = console.ask("Overlord base URL", Some(&default)) else {
        console.blank();
        return Ok(());
    };

    match validate_overlord_url(&answer) {
        Ok(()) => {
            config.settings.overlord.base_url = Some(answer.clone());
            config.save_settings()?;
            outcome.overlord_configured = Some(answer);
            console.say("Saved.");
        }
        Err(message) => {
            console.say(format!("  That URL {message}."));
            console.say("  Skipping; results can still be exported locally.");
        }
    }
    console.blank();
    Ok(())
}

/// Step 6 — install and start the service if the user wants it.
fn step_six_service(
    console: &mut Console<'_>,
    service: std::result::Result<&ServiceManager, &AppError>,
    outcome: &mut SetupOutcome,
) {
    let manager = match service {
        Ok(manager) => manager,
        Err(error) => {
            console.say(format!(
                "  Could not prepare the background service: {}",
                crate::diagnostics::redaction::redact_error(error)
            ));
            console.blank();
            return;
        }
    };

    if !manager.is_supported() {
        console.say(format!(
            "This platform ({}) has no supported service manager.",
            std::env::consts::OS
        ));
        console.say("Run `refinery serve` to keep the daemon running.");
        console.blank();
        return;
    }

    console.say("The background service accepts submissions and runs refinements.");
    console.say("Without it, submissions are only accepted while `refinery serve` is running.");
    if !console.confirm("Install and start it now?", true) {
        console.say("Skipped. Start it later with `refinery service start`.");
        console.blank();
        return;
    }

    match manager.start() {
        Ok(path) => {
            outcome.service_started = true;
            console.say(format!("Started. Unit installed at {}.", path.display()));
        }
        Err(error) => console.say(format!(
            "  Could not start the service: {}. Run `refinery serve` meanwhile.",
            crate::diagnostics::redaction::redact_error(&error)
        )),
    }
    console.blank();
}

/// Generate the local API token if there is not one already.
///
/// This is not one of the seven steps because it is not a decision: a loopback
/// API with no token is an API no client can call, so setup simply provides
/// one. It is announced rather than asked.
fn ensure_api_token(console: &mut Console<'_>, credentials: &CredentialStore) -> Result<()> {
    if credentials
        .contains(&CredentialKey::ApiBearerToken)
        .unwrap_or(false)
    {
        return Ok(());
    }
    credentials.set(&CredentialKey::ApiBearerToken, &generate_api_token())?;
    console.say("Generated a local API token for clients on this machine.");
    console.blank();
    Ok(())
}

/// Ask for a provider API key on its own, outside the guided flow.
///
/// `refinery provider configure` and setup must agree about how a key is
/// prompted for and that it is never echoed, so they share one function rather
/// than two that drift apart.
pub fn read_provider_key(
    console: &mut Console<'_>,
    provider: &str,
    credentials: &CredentialStore,
) -> Result<SecretString> {
    console.say(format!(
        "Paste your {provider} API key. It is stored in the {} and never displayed.",
        credentials.backend().label()
    ));
    console
        .ask_secret(&format!("{provider} API key"))
        .ok_or_else(|| AppError::NeedsUser {
            message: format!("no {provider} API key was entered; nothing was changed"),
        })
}

/// Mint a bearer token for local clients.
///
/// The randomness comes from two v4 UUIDs, which is 244 bits from the platform
/// entropy source — ample for a token that only ever crosses loopback, and it
/// avoids adding a random-number dependency for one call site.
pub fn generate_api_token() -> SecretString {
    SecretString::new(format!(
        "rfy_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// Step 7 — check the result and say how to get back here.
async fn step_seven_health_check(
    config: &Config,
    console: &mut Console<'_>,
    credentials: &CredentialStore,
    service: Option<&ServiceManager>,
    options: &SetupOptions,
    outcome: &mut SetupOutcome,
) {
    console.say("Checking the installation…");
    console.blank();
    let report = doctor::run_with(config, credentials, service, options.offline).await;
    console.say(report.render());
    outcome.health_check_failed = report.has_failures();

    if outcome.health_check_failed {
        console.say("Some checks failed. The lines marked → say what to do.");
    } else {
        console.say("Refinery is ready.");
    }

    console.blank();
    console.say("From here:");
    console.say("  refinery open                    the local interface");
    console.say("  refinery status                  what this installation is doing");
    console.say("  refinery doctor                  re-run these checks");
    console.say("  refinery repository add <path>   ground refinements in another project");
    console.say(format!(
        "  refinery provider configure {}   replace the API key",
        config.settings.provider.backend
    ));
    console.say("  refinery service start|stop      manage the background service");
    console.say("  refinery setup                   run this again");
}

/// Build a console over the process's own standard streams.
pub fn stdio_console<'a>(input: &'a mut dyn BufRead, output: &'a mut dyn Write) -> Console<'a> {
    let interactive = std::io::stdin().is_terminal();
    Console::new(input, output, interactive)
}

/// Refuse to run a flow that needs answers when nothing can supply them.
pub fn require_interactive(what: &str) -> Result<()> {
    if std::io::stdin().is_terminal() {
        return Ok(());
    }
    Err(AppError::NeedsUser {
        message: format!(
            "{what} needs a terminal to ask questions; run it from an interactive shell"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DataDir;

    /// Run setup against a scripted sequence of answers.
    ///
    /// The temporary directory is returned alongside the result: dropping it
    /// deletes the data directory, so a test that inspects what setup wrote
    /// must hold it.
    fn run_setup(
        answers: &[&str],
        options: SetupOptions,
    ) -> (tempfile::TempDir, Config, SetupOutcome, String) {
        let temp = tempfile::tempdir().expect("temp dir");
        let (config, outcome, transcript) = run_setup_in(&temp, answers, options);
        (temp, config, outcome, transcript)
    }

    fn run_setup_in(
        temp: &tempfile::TempDir,
        answers: &[&str],
        mut options: SetupOptions,
    ) -> (Config, SetupOutcome, String) {
        options.force_file_credentials = true;
        options.offline = true;
        if options.unsupervised_service_unit.is_none() {
            options.unsupervised_service_unit =
                Some(temp.path().join("units").join("refinery.unit"));
        }
        if options.working_directory.is_none() {
            options.working_directory = Some(temp.path().join("project"));
            std::fs::create_dir_all(temp.path().join("project")).expect("create project");
        }

        let mut config = Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve"))
            .expect("load");

        let script = answers.join("\n") + "\n";
        let mut input = std::io::Cursor::new(script.into_bytes());
        let mut output = Vec::new();
        let outcome = {
            let mut reader: Box<dyn BufRead> = Box::new(&mut input);
            let mut console = Console::new(&mut reader, &mut output, false);
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(run(&mut config, &mut console, &options))
                .expect("setup runs")
        };

        (config, outcome, String::from_utf8(output).expect("utf8"))
    }

    #[test]
    fn a_clean_machine_is_configured_without_editing_a_file() {
        // The product's completion criterion: setup alone leaves a working
        // installation. Answers: key, register repository, configure Overlord,
        // accept the default URL, decline the service.
        let (_temp, config, outcome, transcript) = run_setup(
            &["AIzaSetupTestKeyValue000", "y", "y", "", "n"],
            SetupOptions::default(),
        );

        assert!(outcome.provider_key_stored, "{transcript}");
        assert!(outcome.repository_registered.is_some(), "{transcript}");
        assert_eq!(
            outcome.overlord_configured.as_deref(),
            Some("http://127.0.0.1:3000"),
            "{transcript}"
        );
        assert_eq!(
            config.settings.overlord.base_url.as_deref(),
            Some("http://127.0.0.1:3000")
        );
        // The setting was persisted, not merely held in memory.
        assert!(config.data_dir.settings_file().exists());

        let store = CredentialStore::file_backed(&config.data_dir.credentials_dir());
        assert!(store
            .contains(&CredentialKey::provider_api_key("gemini"))
            .expect("read"));
        assert!(
            store
                .contains(&CredentialKey::ApiBearerToken)
                .expect("read"),
            "a loopback API with no token is an API nothing can call"
        );
    }

    #[test]
    fn the_key_is_never_echoed_back_to_the_terminal() {
        let secret = "AIzaSetupSecretNeverShown";
        let (_temp, _config, _outcome, transcript) =
            run_setup(&[secret, "n", "n", "n"], SetupOptions::default());
        assert!(
            !transcript.contains(secret),
            "the API key appeared in setup output:\n{transcript}"
        );
    }

    #[test]
    fn the_disclosure_names_what_stays_and_what_is_sent() {
        let (_temp, _config, _outcome, transcript) =
            run_setup(&["", "n", "n", "n"], SetupOptions::default());
        assert!(transcript.contains("What stays on this machine"));
        assert!(transcript.contains("What is sent to the model provider"));
        assert!(transcript.contains("loopback only"));
    }

    #[test]
    fn declining_every_optional_step_still_leaves_a_working_installation() {
        let (_temp, config, outcome, transcript) = run_setup(
            &["AIzaSetupTestKeyValue000", "n", "n", "n"],
            SetupOptions::default(),
        );

        assert!(outcome.provider_key_stored);
        assert!(outcome.repository_registered.is_none());
        assert!(outcome.overlord_configured.is_none());
        assert!(!outcome.service_started);
        // Declining the repository must not have prevented the token from
        // being generated: the steps are independent.
        let store = CredentialStore::file_backed(&config.data_dir.credentials_dir());
        assert!(store
            .contains(&CredentialKey::ApiBearerToken)
            .expect("read"));
        assert!(
            transcript.contains("refinery repository add"),
            "{transcript}"
        );
    }

    #[test]
    fn an_empty_key_leaves_the_installation_usable_and_says_how_to_fix_it() {
        let (_temp, _config, outcome, transcript) =
            run_setup(&["", "n", "n", "n"], SetupOptions::default());
        assert!(!outcome.provider_key_stored);
        assert!(transcript.contains("refinery provider configure gemini"));
    }

    #[test]
    fn setup_is_resumable_and_offers_to_keep_what_is_configured() {
        let temp = tempfile::tempdir().expect("temp dir");
        let first = run_setup_in(
            &temp,
            &["AIzaSetupTestKeyValue000", "y", "y", "", "n"],
            SetupOptions::default(),
        );
        assert!(first.1.provider_key_stored);

        // Second run: keep the key, keep the repository, keep the Overlord URL.
        let (config, outcome, transcript) =
            run_setup_in(&temp, &["n", "y", "n", "n"], SetupOptions::default());

        assert!(
            transcript.contains("already stored"),
            "a second run must recognise the stored key:\n{transcript}"
        );
        assert!(outcome.provider_key_stored);
        assert_eq!(
            config.settings.overlord.base_url.as_deref(),
            Some("http://127.0.0.1:3000"),
            "a configured destination must survive a second run"
        );
    }

    #[test]
    fn a_remote_overlord_url_is_refused_during_setup() {
        let (_temp, config, outcome, transcript) = run_setup(
            &[
                "AIzaSetupTestKeyValue000",
                "n",
                "y",
                "http://overlord.example.com",
                "n",
            ],
            SetupOptions::default(),
        );
        assert!(outcome.overlord_configured.is_none(), "{transcript}");
        assert!(config.settings.overlord.base_url.is_none());
        assert!(transcript.contains("loopback"), "{transcript}");
    }

    #[test]
    fn setup_ends_with_the_health_check_and_the_commands_to_return() {
        let (_temp, _config, outcome, transcript) = run_setup(
            &["AIzaSetupTestKeyValue000", "y", "n", "n"],
            SetupOptions::default(),
        );
        assert!(transcript.contains("passed,"), "{transcript}");
        assert!(transcript.contains("refinery doctor"), "{transcript}");
        assert!(transcript.contains("refinery status"), "{transcript}");
        assert!(transcript.contains("refinery setup"), "{transcript}");
        // Offline, the provider connection is untested, which doctor reports
        // as a warning rather than a failure.
        assert!(!outcome.health_check_failed, "{transcript}");
    }

    #[test]
    fn running_out_of_input_takes_the_defaults_rather_than_hanging() {
        // A piped or truncated script must still terminate.
        let (_temp, _config, outcome, transcript) = run_setup(&[], SetupOptions::default());
        assert!(!outcome.provider_key_stored, "{transcript}");
        assert!(
            transcript.contains("Checking the installation"),
            "{transcript}"
        );
    }

    #[test]
    fn a_generated_token_is_long_and_distinct_each_time() {
        let first = generate_api_token();
        let second = generate_api_token();
        assert!(first.expose().starts_with("rfy_"));
        assert!(first.expose().len() >= 36);
        assert_ne!(first.expose(), second.expose());
    }
}
