//! Configuration: where Refinery's data lives and what the non-secret
//! settings say.
//!
//! Configuration never holds secrets. API keys and the local bearer token are
//! kept in the operating-system credential store (with a `0600` fallback file
//! inside the data directory where no store exists), and are referenced by
//! name from here.

pub mod credentials;
pub mod paths;
pub mod settings;

pub use credentials::{CredentialBackend, CredentialKey, CredentialStore};
pub use paths::DataDir;
pub use settings::Settings;

use crate::error::Result;

/// The loaded configuration for one process.
#[derive(Debug, Clone)]
pub struct Config {
    /// Resolved data-directory layout.
    pub data_dir: DataDir,
    /// Non-secret settings.
    pub settings: Settings,
}

impl Config {
    /// Resolve the data directory, create it with owner-only permissions, and
    /// load the settings file.
    ///
    /// This is the single entry point every command uses, so the data
    /// directory exists and is private before anything writes to it.
    pub fn load() -> Result<Self> {
        Self::load_from(DataDir::resolve()?)
    }

    /// Load configuration rooted at an explicit data directory.
    pub fn load_from(data_dir: DataDir) -> Result<Self> {
        data_dir.ensure()?;
        let settings = Settings::load(&data_dir.settings_file())?;
        Ok(Self { data_dir, settings })
    }

    /// Persist the current settings to the data directory.
    pub fn save_settings(&self) -> Result<()> {
        self.settings.save(&self.data_dir.settings_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_creates_the_data_directory_and_uses_defaults() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::at(temp.path().join("data")).expect("resolve");

        let config = Config::load_from(data_dir).expect("load");

        assert!(config.data_dir.root().is_dir());
        assert!(config.data_dir.log_dir().is_dir());
        assert_eq!(config.settings, Settings::default());
        assert!(!config.data_dir.settings_file().exists());
    }

    #[test]
    fn saved_settings_are_read_back() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::at(temp.path().join("data")).expect("resolve");

        let mut config = Config::load_from(data_dir.clone()).expect("load");
        config.settings.api.port = 4321;
        config.save_settings().expect("save");

        let reloaded = Config::load_from(data_dir).expect("reload");
        assert_eq!(reloaded.settings.api.port, 4321);
    }
}
