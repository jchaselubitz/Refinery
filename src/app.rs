//! The composition root.
//!
//! Everything a command needs is assembled here in one order: resolve the data
//! directory, create it privately, load settings, install logging, then
//! dispatch. Commands never resolve configuration themselves, so there is a
//! single place where the data directory is created and secured.

use crate::cli::Cli;
use crate::config::Config;
use crate::diagnostics::logging;
use crate::error::Result;
use crate::storage::Storage;

/// Run one CLI invocation.
pub async fn run(cli: Cli) -> Result<()> {
    let data_dir = cli.data_dir()?;
    let mut config = Config::load_from(data_dir)?;

    if let Some(filter) = cli.log.clone() {
        config.settings.logging.filter = filter;
    }

    // The handle owns the background log writer; holding it until the command
    // returns keeps file logging alive and flushes it on the way out.
    let _logging = logging::init(&config.data_dir, &config.settings.logging)?;

    tracing::debug!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %config.data_dir.root().display(),
        "starting"
    );

    // Opening the store applies migrations and runs recovery before any
    // command can observe or start work. The actual job handlers arrive with
    // the provider and delivery milestones, but recovery is deliberately
    // ready now so a later daemon never has to guess about abandoned leases.
    let store = Storage::open(config.data_dir.database_file()).await?;
    let resumed = store.recover_startup(chrono::Utc::now()).await?;
    tracing::debug!(resumed_cases = resumed, "durable startup recovery complete");

    if matches!(cli.command, crate::cli::Command::Serve) {
        return crate::api::serve(&config, store).await;
    }
    crate::cli::run(cli.command, config).await
}
