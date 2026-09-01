//! The `refinery` command-line surface.
//!
//! The command tree is complete from the start so the shape of the product is
//! visible and `--help` is honest about what exists. Commands that a later
//! milestone delivers fail with [`crate::error::AppError::Unsupported`] naming
//! that milestone, rather than silently doing nothing.

pub mod setup;
pub mod uninstall;
pub mod update;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::credentials::{CredentialKey, CredentialStore};
use crate::config::{Config, DataDir};
use crate::diagnostics::doctor;
use crate::diagnostics::service::ServiceManager;
use crate::error::{AppError, Result};
use crate::storage::Storage;

/// Turn transcripts, repository context, and media into validated prompts.
#[derive(Debug, Parser)]
#[command(name = "refinery", version, about, long_about = None)]
pub struct Cli {
    /// Override the data directory. Defaults to the platform application data
    /// location.
    #[arg(long, global = true, env = crate::config::paths::DATA_DIR_ENV, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// Override the log filter, for example `info` or `refinery=debug`.
    #[arg(long, global = true, env = crate::diagnostics::logging::LOG_FILTER_ENV, value_name = "FILTER")]
    pub log: Option<String>,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the guided first-run setup.
    Setup {
        /// Skip every network call, including the provider connection test.
        #[arg(long)]
        offline: bool,
    },

    /// Open the local browser interface.
    Open {
        /// Print the tokened URL instead of launching a browser.
        #[arg(long)]
        print: bool,
    },

    /// Show configuration, service, and case status.
    Status,

    /// Check the installation and report what to fix.
    Doctor {
        /// Skip checks that need the network.
        #[arg(long)]
        offline: bool,
    },

    /// Run the daemon in the foreground.
    Serve,

    /// Manage registered repositories.
    Repository {
        /// The repository action.
        #[command(subcommand)]
        action: RepositoryCommand,
    },

    /// Manage model providers.
    Provider {
        /// The provider action.
        #[command(subcommand)]
        action: ProviderCommand,
    },

    /// Manage the background service.
    Service {
        /// The service action.
        #[command(subcommand)]
        action: ServiceCommand,
    },

    /// Replace this installation with the newest published release.
    ///
    /// The archive is checked against the checksums the release published, its
    /// payload manifest, and — on a signed install — the signing team, before
    /// anything on disk is touched. Copies owned by a package manager are
    /// refused rather than silently diverged from their package.
    Update {
        /// Report what is available without installing it.
        #[arg(long)]
        check: bool,
        /// Install the published release even when it is not newer.
        #[arg(long)]
        force: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Remove this installation from the machine.
    ///
    /// Stops and unregisters the background service, deletes the stored
    /// provider key and loopback token, and removes the executable. The data
    /// directory is kept unless `--purge` asks for it.
    Uninstall {
        /// Also delete the data directory: settings, database, media, logs.
        #[arg(long)]
        purge: bool,
        /// Leave the executable in place.
        #[arg(long)]
        keep_binary: bool,
        /// Proceed without asking for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Repository management actions.
#[derive(Debug, Subcommand)]
pub enum RepositoryCommand {
    /// Register a repository root for read-only grounding.
    Add {
        /// The repository root. Defaults to the current directory.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List registered repositories.
    List,
    /// Stop reading a repository, leaving the directory untouched.
    Remove {
        /// The repository root to forget.
        path: PathBuf,
    },
}

/// Provider management actions.
#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Configure credentials for a provider.
    Configure {
        /// The provider identifier, for example `gemini`.
        provider: String,
        /// Store the key without testing it against the provider.
        #[arg(long)]
        offline: bool,
    },
}

/// Background-service actions.
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Install if needed and start the background service.
    Start,
    /// Stop the background service.
    Stop,
    /// Report what the platform supervisor says about the service.
    Status,
}

impl Cli {
    /// Resolve the data directory this invocation should use, honouring the
    /// `--data-dir` flag before the platform default.
    pub fn data_dir(&self) -> Result<DataDir> {
        match &self.data_dir {
            Some(path) => DataDir::at(path.clone()),
            None => DataDir::resolve(),
        }
    }
}

/// Run one command against a loaded configuration.
pub async fn run(command: Command, mut config: Config) -> Result<()> {
    match command {
        Command::Setup { offline } => run_setup(&mut config, offline).await,
        Command::Status => {
            let credentials = CredentialStore::open(&config.data_dir.credentials_dir());
            status(&config, &credentials).await
        }
        Command::Doctor { offline } => run_doctor(&config, offline).await,
        Command::Provider { action } => provider(&config, action).await,
        Command::Service { action } => service(&config, action),
        Command::Repository { action } => repository(&config, action).await,
        // `serve` is intercepted in `app::run`, which owns the store and the
        // job loop the daemon needs. Reaching here means that interception was
        // removed, and failing loudly beats serving nothing.
        Command::Serve => Err(AppError::Internal(anyhow::anyhow!(
            "serve must be dispatched by the application root"
        ))),
        Command::Open { print } => open(&config, print),
        Command::Update { check, force, json } => run_update(check, force, json).await,
        Command::Uninstall {
            purge,
            keep_binary,
            yes,
            json,
        } => run_uninstall(&config, purge, keep_binary, yes, json),
    }
}

/// Check for, and unless asked not to install, the newest published release.
async fn run_update(check: bool, force: bool, json: bool) -> Result<()> {
    let report = update::update(update::UpdateOptions {
        check_only: check,
        force,
        ..update::UpdateOptions::new(env!("CARGO_PKG_VERSION"))
    })
    .await?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|source| AppError::Internal(source.into()))?
        );
        return Ok(());
    }

    // The human form says what happened and, when something is available, how
    // to get it: a check that only prints a version number leaves the reader to
    // guess the command.
    let latest = report.latest_version.as_deref().unwrap_or("unknown");
    match report.status.as_str() {
        "installed" => {
            println!("Updated {} to {latest}.", report.current_version);
            if let Some(path) = &report.installed_path {
                println!("{}", path.display());
            }
        }
        "available" => {
            println!(
                "refinery {latest} is available (this is {}); run `refinery update` to install it.",
                report.current_version
            );
            if let Some(url) = &report.release_url {
                println!("{url}");
            }
        }
        _ => println!("refinery {} is the latest release.", report.current_version),
    }
    Ok(())
}

/// Remove this installation, after saying exactly what will go.
fn run_uninstall(
    config: &Config,
    purge: bool,
    keep_binary: bool,
    yes: bool,
    json: bool,
) -> Result<()> {
    let options = uninstall::UninstallOptions {
        purge,
        keep_binary,
        assume_yes: yes,
        executable: None,
    };

    if !yes {
        // A prompt nobody can answer is a hang, and an uninstall that proceeds
        // unasked is worse, so a non-interactive run without `--yes` stops.
        setup::require_interactive("refinery uninstall")?;

        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        let mut console = setup::stdio_console(&mut input, &mut output);

        println!("This will stop the service and remove the stored provider key and API token.");
        if purge {
            println!(
                "It will also delete {} — settings, cases, media, and logs.",
                config.data_dir.root().display()
            );
        }
        if !console.confirm("Remove this Refinery installation?", false) {
            println!("Nothing was removed.");
            return Ok(());
        }
    }

    let credentials = CredentialStore::open(&config.data_dir.credentials_dir());
    let service = ServiceManager::new(config.data_dir.clone())?;
    let report = uninstall::uninstall(config, &credentials, &service, &options)?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|source| AppError::Internal(source.into()))?
        );
    } else {
        println!("{}", uninstall::render(&report));
    }
    Ok(())
}

/// The guided first run.
async fn run_setup(config: &mut Config, offline: bool) -> Result<()> {
    setup::require_interactive("refinery setup")?;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut console = setup::stdio_console(&mut input, &mut output);

    let options = setup::SetupOptions {
        offline,
        ..setup::SetupOptions::default()
    };
    let outcome = setup::run(config, &mut console, &options).await?;

    // Setup itself succeeded — every step ran — but a failed health check
    // means the installation is not usable yet, and the exit status should say
    // so for anyone scripting around it.
    if outcome.health_check_failed {
        return Err(AppError::NeedsUser {
            message: "setup finished, but some checks failed; see the remedies above".to_owned(),
        });
    }
    Ok(())
}

/// Run every diagnostic check and exit non-zero if any failed.
async fn run_doctor(config: &Config, offline: bool) -> Result<()> {
    let report = doctor::run(config, offline).await;
    print!("{}", report.render());

    if report.has_failures() {
        return Err(AppError::NeedsUser {
            message: format!(
                "{} check(s) failed; the lines marked → say what to do",
                report.count(doctor::Status::Fail)
            ),
        });
    }
    Ok(())
}

/// Store and verify a provider credential.
async fn provider(config: &Config, action: ProviderCommand) -> Result<()> {
    let ProviderCommand::Configure { provider, offline } = action;

    if provider != config.settings.provider.backend {
        return Err(AppError::invalid(format!(
            "this installation is configured for `{}`; set provider.backend in {} to configure `{provider}`",
            config.settings.provider.backend,
            config.data_dir.settings_file().display()
        )));
    }

    setup::require_interactive("refinery provider configure")?;

    let credentials = CredentialStore::open(&config.data_dir.credentials_dir());
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut console = setup::stdio_console(&mut input, &mut output);

    let secret = setup::read_provider_key(&mut console, &provider, &credentials)?;
    credentials.set(&CredentialKey::provider_api_key(&provider), &secret)?;
    println!("Stored in the {}.", credentials.backend().label());

    if offline {
        println!("Not tested (offline). Run `refinery doctor` when you are online.");
        return Ok(());
    }

    let connection = crate::agent::test_gemini(
        crate::agent::GEMINI_API_BASE,
        &config.settings.provider.model,
        &secret,
    )
    .await?;
    println!("Connected to {}.", connection.summary());
    Ok(())
}

/// Launch a browser against the local interface.
///
/// The URL carries the bearer token as a query parameter because that is the
/// only channel a browser launch has: a command line cannot set a request
/// header, and the interface's very first request has to be authenticated or
/// there is nothing to render. The page moves the token into session storage
/// and rewrites the address bar on load, so it does not survive in history.
///
/// `--print` exists for the cases where handing someone a URL is better than
/// opening a window: a remote shell, a container, a second browser profile.
fn open(config: &Config, print: bool) -> Result<()> {
    let token = crate::api::ensure_token(config)?;
    let address = config.settings.api.bind_address();
    let url = interface_url(config, &token);

    // The token is a secret, and the redaction layer is what keeps it out of
    // logs and error text everywhere else. Registering it here means a URL
    // that reaches a log through some later path is redacted too.
    crate::diagnostics::redaction::register_secret(&token);

    if print {
        println!("{url}");
        return Ok(());
    }

    match browser_command(&url) {
        Some((program, arguments)) => {
            let status = std::process::Command::new(&program)
                .args(&arguments)
                .status()
                .map_err(|source| AppError::Io {
                    path: std::path::PathBuf::from(&program),
                    source,
                })?;
            if !status.success() {
                return Err(AppError::NeedsUser {
                    message: format!(
                        "could not launch a browser with `{program}`; open this address yourself:\n  {url}"
                    ),
                });
            }
            println!("Opened http://{address}/ in your browser.");
            Ok(())
        }
        None => {
            println!("Open this address in a browser:");
            println!("  {url}");
            Ok(())
        }
    }
}

/// The loopback address of the interface, carrying the token.
fn interface_url(config: &Config, token: &str) -> String {
    format!(
        "http://{}/?token={}",
        config.settings.api.bind_address(),
        url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>()
    )
}

/// The platform's "open this URL" command, when there is one.
fn browser_command(url: &str) -> Option<(String, Vec<String>)> {
    if let Ok(program) = std::env::var("REFINERY_BROWSER") {
        if !program.is_empty() {
            return Some((program, vec![url.to_owned()]));
        }
    }
    if cfg!(target_os = "macos") {
        Some(("open".to_owned(), vec![url.to_owned()]))
    } else if cfg!(target_os = "windows") {
        Some((
            "cmd".to_owned(),
            vec![
                "/C".to_owned(),
                "start".to_owned(),
                String::new(),
                url.to_owned(),
            ],
        ))
    } else if cfg!(target_os = "linux") {
        Some(("xdg-open".to_owned(), vec![url.to_owned()]))
    } else {
        None
    }
}

/// Manage the platform background service.
fn service(config: &Config, action: ServiceCommand) -> Result<()> {
    let manager = ServiceManager::new(config.data_dir.clone())?;
    match action {
        ServiceCommand::Start => {
            let path = manager.start()?;
            println!("Service started. Unit installed at {}.", path.display());
            println!(
                "Listening on http://{}.",
                config.settings.api.bind_address()
            );
        }
        ServiceCommand::Stop => {
            manager.stop()?;
            println!("Service stopped.");
        }
        ServiceCommand::Status => println!("{}", manager.status().describe()),
    }
    Ok(())
}

/// Register, list, and forget repository roots.
async fn repository(config: &Config, action: RepositoryCommand) -> Result<()> {
    let storage = Storage::open(config.data_dir.database_file()).await?;

    match action {
        RepositoryCommand::Add { path } => {
            let root = crate::repositories::canonical_root(&path)?;
            let policy =
                crate::repositories::RepositoryPolicy::from_limits(&config.settings.limits);
            let (repository, created) = storage
                .register_repository(&root, &policy, chrono::Utc::now())
                .await?;

            println!(
                "{} {}",
                if created {
                    "Registered"
                } else {
                    "Already registered; policy refreshed for"
                },
                repository.root.display()
            );
            println!(
                "  read-only, up to {} bytes per file, {} results per call",
                repository.policy.max_file_bytes, repository.policy.max_results
            );
            if !repository.is_git_work_tree() {
                println!("  not a Git working tree: git_status and git_diff are unavailable here");
            }
        }
        RepositoryCommand::List => {
            let repositories = storage.list_repositories().await?;
            if repositories.is_empty() {
                println!("No repositories registered. Add one with `refinery repository add .`.");
                return Ok(());
            }
            for repository in repositories {
                println!(
                    "{}  {}{}",
                    repository.root.display(),
                    if repository.is_reachable() {
                        "reachable"
                    } else {
                        "MISSING"
                    },
                    if repository.is_git_work_tree() {
                        ", git"
                    } else {
                        ""
                    }
                );
            }
        }
        RepositoryCommand::Remove { path } => {
            // The path is canonicalized when it still exists, so `remove` works
            // with the same spelling `add` accepted; a directory that has since
            // been deleted is matched as given.
            let root = crate::repositories::canonical_root(&path).unwrap_or(path);
            if storage.forget_repository(&root).await? {
                println!("Forgot {}. The directory was not modified.", root.display());
            } else {
                println!("{} was not registered.", root.display());
            }
        }
    }
    Ok(())
}

/// Print what this installation is configured to do.
/// The credential store is passed in rather than opened here so a test can
/// exercise `status` without reading the credential store of the machine
/// running it — which, on macOS, can block on an authorization prompt.
async fn status(config: &Config, credentials: &CredentialStore) -> Result<()> {
    let data_dir = &config.data_dir;
    let settings = &config.settings;

    println!("refinery {}", env!("CARGO_PKG_VERSION"));
    println!("data directory  {}", data_dir.root().display());
    println!(
        "settings        {} ({})",
        data_dir.settings_file().display(),
        if data_dir.settings_file().exists() {
            "present"
        } else {
            "defaults, not yet written"
        }
    );
    println!(
        "database        {} ({})",
        data_dir.database_file().display(),
        if data_dir.database_file().exists() {
            "present"
        } else {
            "not created yet"
        }
    );
    println!("logs            {}", data_dir.log_dir().display());
    println!("api             http://{}", settings.api.bind_address());
    println!(
        "provider        {} ({}, key {})",
        settings.provider.backend,
        settings.provider.model,
        match credentials.contains(&CredentialKey::provider_api_key(&settings.provider.backend)) {
            Ok(true) => "stored",
            Ok(false) => "not stored",
            Err(_) => "unreadable",
        }
    );
    println!("credentials     {}", credentials.backend().label());
    println!(
        "overlord        {}",
        settings
            .overlord
            .base_url
            .as_deref()
            .unwrap_or("not configured")
    );
    println!(
        "service         {}",
        match ServiceManager::new(data_dir.clone()) {
            Ok(manager) => manager.status().describe(),
            Err(error) => crate::diagnostics::redaction::redact_error(&error),
        }
    );

    // The counts come last: they need the database, and everything above is
    // still worth printing on an installation whose database cannot be opened.
    let storage = Storage::open(data_dir.database_file()).await?;
    let repositories = storage.list_repositories().await?;
    println!("repositories    {}", repositories.len());

    let cases = storage.case_counts_by_state().await?;
    let total: u64 = cases.values().sum();
    println!("cases           {total}");
    for (state, count) in &cases {
        println!("  {state:<14}{count}");
    }

    let jobs = storage.job_counts_by_status().await?;
    let pending: u64 = jobs
        .iter()
        .filter(|(status, _)| status.as_str() == "queued" || status.as_str() == "leased")
        .map(|(_, count)| *count)
        .sum();
    println!("jobs            {pending} pending");

    let metrics = storage.product_metrics().await?;
    println!("product metrics");
    println!(
        "  delivered      {} / {} ({})",
        metrics.delivered_cases,
        metrics.submitted_cases,
        percentage(metrics.delivered_cases, metrics.submitted_cases)
    );
    println!(
        "  question rate  {} / {} cases ({})",
        metrics.cases_with_questions,
        metrics.submitted_cases,
        percentage(metrics.cases_with_questions, metrics.submitted_cases)
    );
    println!(
        "  answer resume  {} / {} ({})",
        metrics.resumed_answers,
        metrics.answered_question_requests,
        percentage(metrics.resumed_answers, metrics.answered_question_requests)
    );
    println!(
        "  deliveries     {} logical, {} attempts, {} retries",
        metrics.deliveries, metrics.delivery_attempts, metrics.delivery_retries
    );
    println!(
        "  validation     {} / {} candidates failed ({})",
        metrics.validation_failures,
        metrics.candidate_outputs,
        percentage(metrics.validation_failures, metrics.candidate_outputs)
    );

    Ok(())
}

fn percentage(numerator: u64, denominator: u64) -> String {
    crate::storage::ProductMetrics::percent(numerator, denominator)
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "n/a".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_renders() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("refinery"));
        assert!(help.contains("doctor"));
    }

    #[test]
    fn the_data_dir_flag_wins_over_the_platform_default() {
        let cli = Cli::try_parse_from(["refinery", "--data-dir", "/tmp/refinery-test", "status"])
            .expect("parse");
        let dir = cli.data_dir().expect("resolve");
        assert_eq!(dir.root(), std::path::Path::new("/tmp/refinery-test"));
    }

    #[test]
    fn repository_add_defaults_to_the_current_directory() {
        let cli = Cli::try_parse_from(["refinery", "repository", "add"]).expect("parse");
        match cli.command {
            Command::Repository {
                action: RepositoryCommand::Add { path },
            } => assert_eq!(path, PathBuf::from(".")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn doctor_and_setup_accept_an_offline_flag() {
        // `doctor --offline` is what makes the command usable on a machine with
        // no network, so the flag is part of the surface, not an internal.
        let cli = Cli::try_parse_from(["refinery", "doctor", "--offline"]).expect("parse");
        assert!(matches!(cli.command, Command::Doctor { offline: true }));
        let cli = Cli::try_parse_from(["refinery", "setup"]).expect("parse");
        assert!(matches!(cli.command, Command::Setup { offline: false }));
    }

    /// `serve` must be dispatched by the application root, which owns the
    /// store and the job loop. If it ever reaches the ordinary dispatcher the
    /// daemon would bind a listener with nothing running behind it, so this
    /// asserts the guard rather than the message.
    #[tokio::test]
    async fn serve_is_never_dispatched_by_the_ordinary_command_path() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve"))
            .expect("load");

        let error = run(Command::Serve, config).await.expect_err("guarded");
        assert_eq!(error.code(), "internal_error");
    }

    #[test]
    fn the_interface_url_is_loopback_and_carries_an_escaped_token() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve"))
            .expect("load");

        let url = interface_url(&config, "a token/with=specials");
        assert!(url.starts_with("http://127.0.0.1:"), "not loopback: {url}");
        assert!(url.contains("?token=a+token%2Fwith%3Dspecials"), "{url}");
        assert!(!url.contains(' '));
    }

    #[test]
    fn a_configured_browser_overrides_the_platform_default() {
        // Not run in parallel with anything that reads the same variable: the
        // override exists for containers and remote shells, where the platform
        // default is either absent or wrong.
        std::env::set_var("REFINERY_BROWSER", "my-browser");
        let (program, arguments) = browser_command("http://127.0.0.1:8765/").expect("a command");
        std::env::remove_var("REFINERY_BROWSER");
        assert_eq!(program, "my-browser");
        assert_eq!(arguments, vec!["http://127.0.0.1:8765/".to_owned()]);
    }

    #[tokio::test]
    async fn configuring_a_provider_this_build_is_not_set_up_for_is_refused() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve"))
            .expect("load");

        let error = run(
            Command::Provider {
                action: ProviderCommand::Configure {
                    provider: "openai".into(),
                    offline: true,
                },
            },
            config,
        )
        .await
        .expect_err("must refuse");
        assert_eq!(error.code(), "invalid_request");
        assert!(error.to_string().contains("provider.backend"));
    }

    #[tokio::test]
    async fn repositories_can_be_added_listed_and_forgotten() {
        let temp = tempfile::tempdir().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("create");
        let config = Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve"))
            .expect("load");

        repository(
            &config,
            RepositoryCommand::Add {
                path: project.clone(),
            },
        )
        .await
        .expect("add");

        let storage = Storage::open(config.data_dir.database_file())
            .await
            .expect("open");
        assert_eq!(storage.list_repositories().await.expect("list").len(), 1);

        // Adding the same root twice is one repository, not two views of it.
        repository(
            &config,
            RepositoryCommand::Add {
                path: project.clone(),
            },
        )
        .await
        .expect("re-add");
        assert_eq!(storage.list_repositories().await.expect("list").len(), 1);

        repository(&config, RepositoryCommand::List)
            .await
            .expect("list");
        repository(
            &config,
            RepositoryCommand::Remove {
                path: project.clone(),
            },
        )
        .await
        .expect("remove");
        assert!(storage.list_repositories().await.expect("list").is_empty());
        // The directory itself is untouched.
        assert!(project.is_dir());
    }

    #[tokio::test]
    async fn adding_a_missing_repository_explains_why_not() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve"))
            .expect("load");

        let error = repository(
            &config,
            RepositoryCommand::Add {
                path: temp.path().join("absent"),
            },
        )
        .await
        .expect_err("must refuse");
        assert_eq!(error.code(), "invalid_request");
    }

    #[tokio::test]
    async fn status_reports_a_fresh_installation() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config::load_from(DataDir::at(temp.path().join("data")).expect("resolve"))
            .expect("load");

        let credentials = CredentialStore::file_backed(&config.data_dir.credentials_dir());
        status(&config, &credentials).await.expect("status runs");
    }

    #[test]
    fn percentages_are_stable_and_empty_samples_are_explicit() {
        assert_eq!(percentage(1, 4), "25.0%");
        assert_eq!(percentage(0, 0), "n/a");
    }
}
