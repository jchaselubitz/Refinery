//! The `refinery` executable.

use clap::Parser;

use refinery::cli::Cli;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    match refinery::app::run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Logging may not be installed yet when configuration itself
            // failed, so the failure always goes to stderr directly.
            eprintln!("refinery: {error}");
            if let Some(source) = std::error::Error::source(&error) {
                eprintln!("  caused by: {source}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}
