//! Data-directory resolution and the on-disk layout Refinery owns.

use std::path::{Path, PathBuf};

use crate::error::{AppError, Result};

/// Environment variable that overrides the platform data directory.
pub const DATA_DIR_ENV: &str = "REFINERY_DATA_DIR";

/// File name of the non-secret settings file inside the data directory.
pub const SETTINGS_FILE: &str = "refinery.toml";

/// Permissions applied to directories Refinery creates (owner-only).
#[cfg(unix)]
pub const DIR_MODE: u32 = 0o700;

/// Permissions applied to files Refinery creates (owner-only).
#[cfg(unix)]
pub const FILE_MODE: u32 = 0o600;

/// The resolved location of every path Refinery owns.
///
/// A `DataDir` value only describes locations; nothing is created until
/// [`DataDir::ensure`] runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Resolve the data directory from the environment, falling back to the
    /// platform-appropriate application data location.
    ///
    /// The `REFINERY_DATA_DIR` override is expanded to an absolute path so
    /// later canonical-path checks have a stable base even when the process
    /// changes its working directory.
    pub fn resolve() -> Result<Self> {
        match std::env::var_os(DATA_DIR_ENV) {
            Some(value) if !value.is_empty() => Self::at(PathBuf::from(value)),
            _ => Ok(Self::new(platform_default()?)),
        }
    }

    /// Resolve the data directory rooted at an explicit path.
    pub fn at(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let absolute = if path.is_absolute() {
            path
        } else {
            let cwd = std::env::current_dir().map_err(|source| AppError::io(".", source))?;
            cwd.join(path)
        };
        Ok(Self::new(absolute))
    }

    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The data-directory root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The non-secret settings file.
    pub fn settings_file(&self) -> PathBuf {
        self.root.join(SETTINGS_FILE)
    }

    /// The SQLite database file.
    pub fn database_file(&self) -> PathBuf {
        self.root.join("refinery.db")
    }

    /// The root of the case-scoped media store.
    pub fn media_dir(&self) -> PathBuf {
        self.root.join("media")
    }

    /// The media directory for one case.
    pub fn case_media_dir(&self, case_id: &str) -> PathBuf {
        self.media_dir().join(case_id)
    }

    /// The rotating log directory.
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The directory holding the credential-store fallback file, used only
    /// where the operating system exposes no credential store.
    pub fn credentials_dir(&self) -> PathBuf {
        self.root.join("credentials")
    }

    /// The directory holding runtime state such as the service lock.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// Create the data directory and its subdirectories with owner-only
    /// permissions. Existing directories have their permissions tightened, so
    /// a directory loosened after installation is repaired on the next start.
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.media_dir(),
            self.log_dir(),
            self.credentials_dir(),
            self.run_dir(),
        ] {
            ensure_private_dir(&dir)?;
        }
        Ok(())
    }
}

/// Create a directory if needed and restrict it to the owner.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path).map_err(|source| AppError::io(path, source))?;
    }
    set_private_dir_mode(path)
}

#[cfg(unix)]
fn set_private_dir_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::Permissions::from_mode(DIR_MODE);
    std::fs::set_permissions(path, permissions).map_err(|source| AppError::io(path, source))
}

#[cfg(not(unix))]
fn set_private_dir_mode(_path: &Path) -> Result<()> {
    // Windows inherits the user profile's access control, which is already
    // owner-scoped; there is no mode bit to set.
    Ok(())
}

/// Restrict an existing file to the owner.
#[cfg(unix)]
pub fn set_private_file_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::Permissions::from_mode(FILE_MODE);
    std::fs::set_permissions(path, permissions).map_err(|source| AppError::io(path, source))
}

/// Restrict an existing file to the owner.
#[cfg(not(unix))]
pub fn set_private_file_mode(_path: &Path) -> Result<()> {
    Ok(())
}

fn platform_default() -> Result<PathBuf> {
    directories::ProjectDirs::from("", "", "refinery")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .ok_or_else(|| {
            AppError::config(format!(
                "could not determine a home directory for the data directory; set {DATA_DIR_ENV}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_root_defines_the_layout() {
        let dir = DataDir::at("/var/refinery").expect("absolute path resolves");
        assert_eq!(dir.root(), Path::new("/var/refinery"));
        assert_eq!(
            dir.settings_file(),
            Path::new("/var/refinery/refinery.toml")
        );
        assert_eq!(dir.database_file(), Path::new("/var/refinery/refinery.db"));
        assert_eq!(
            dir.case_media_dir("case-1"),
            Path::new("/var/refinery/media/case-1")
        );
    }

    #[test]
    fn relative_roots_become_absolute() {
        let dir = DataDir::at("relative-root").expect("relative path resolves");
        assert!(dir.root().is_absolute());
        assert!(dir.root().ends_with("relative-root"));
    }

    #[test]
    fn ensure_creates_the_tree_and_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let dir = DataDir::at(temp.path().join("data")).expect("resolve");

        dir.ensure().expect("first ensure");
        dir.ensure().expect("second ensure");

        assert!(dir.root().is_dir());
        assert!(dir.media_dir().is_dir());
        assert!(dir.log_dir().is_dir());
        assert!(dir.credentials_dir().is_dir());
        assert!(dir.run_dir().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_tightens_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let dir = DataDir::at(temp.path().join("data")).expect("resolve");
        dir.ensure().expect("ensure");

        std::fs::set_permissions(dir.root(), std::fs::Permissions::from_mode(0o755))
            .expect("loosen permissions");
        dir.ensure().expect("re-ensure");

        let mode = std::fs::metadata(dir.root())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, DIR_MODE);
    }
}
