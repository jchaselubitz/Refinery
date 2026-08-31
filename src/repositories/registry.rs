//! Repository registration: which roots the connector may read, and under
//! what policy.
//!
//! Registration is where a repository's boundary is decided, once. The path
//! the user typed is canonicalized here — symlinks resolved, `..` collapsed,
//! relative paths made absolute — and only the canonical form is stored. Every
//! later access check in M4 compares against that stored root, so a symlink
//! swapped after registration cannot widen what the connector may read.
//!
//! The policy is stored alongside the root rather than read from settings at
//! access time. A user who registers a repository under one set of bounds
//! should not have those bounds silently changed by a later edit to the
//! settings file, and a policy that travels with the row survives export and
//! restore.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::settings::LimitSettings;
use crate::domain::RepositoryId;
use crate::error::{AppError, Result};

/// The access policy stored with a registered repository.
///
/// M4 adds the enforcement; M3 fixes the shape and the defaults so a
/// repository registered today is still readable by the connector when it
/// arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPolicy {
    /// Largest single file a read tool will return, in bytes.
    pub max_file_bytes: u64,
    /// Largest number of entries any tool returns in one call.
    pub max_results: usize,
    /// Whether `.gitignore` and friends are honoured during traversal.
    #[serde(default = "default_true")]
    pub respect_ignore_files: bool,
    /// Globs that are never listed, searched, or read, regardless of ignore
    /// files. Defaults exclude the file names that most often hold secrets.
    #[serde(default = "default_secret_exclusions")]
    pub secret_exclusions: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Files that are excluded from a repository by default.
///
/// This is a floor, not a guarantee: a secret can live anywhere, and the
/// product's real protection is that repository content is never treated as
/// instruction. These patterns simply keep the obvious cases out of a prompt.
fn default_secret_exclusions() -> Vec<String> {
    [
        "**/.env",
        "**/.env.*",
        "**/*.pem",
        "**/*.key",
        "**/*.p12",
        "**/*.pfx",
        "**/id_rsa",
        "**/id_ecdsa",
        "**/id_ed25519",
        "**/.npmrc",
        "**/.netrc",
        "**/.aws/credentials",
        "**/secrets.*",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

impl RepositoryPolicy {
    /// The policy implied by an installation's configured limits.
    pub fn from_limits(limits: &LimitSettings) -> Self {
        Self {
            max_file_bytes: limits.max_file_bytes,
            max_results: limits.max_results,
            respect_ignore_files: true,
            secret_exclusions: default_secret_exclusions(),
        }
    }
}

impl Default for RepositoryPolicy {
    fn default() -> Self {
        Self::from_limits(&LimitSettings::default())
    }
}

/// A repository root Refinery is allowed to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    /// Durable identity.
    pub id: RepositoryId,
    /// The canonical root. Always absolute, always symlink-resolved.
    pub root: PathBuf,
    /// The access policy fixed at registration.
    pub policy: RepositoryPolicy,
    /// When the repository was first registered.
    pub registered_at: chrono::DateTime<chrono::Utc>,
}

impl Repository {
    /// A short label for CLI and UI listings: the directory's own name.
    pub fn label(&self) -> String {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Whether the root still exists and is a directory.
    ///
    /// A registered repository can be moved or deleted between runs, which is
    /// exactly what `doctor`'s reachability check reports.
    pub fn is_reachable(&self) -> bool {
        self.root.is_dir()
    }

    /// Whether the root is a Git working tree.
    ///
    /// The `git_status` and `git_diff` tools need one; the other three do not,
    /// so a non-Git directory is registrable and merely offers fewer tools.
    pub fn is_git_work_tree(&self) -> bool {
        self.root.join(".git").exists()
    }
}

/// Canonicalize a user-supplied repository path into a root that can be stored.
///
/// Rejecting here rather than at first use means the user learns immediately
/// that they pointed at a file, a missing directory, or something they cannot
/// read — while they still have the command they typed in front of them.
pub fn canonical_root(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => AppError::invalid(format!(
            "{} does not exist, so it cannot be registered as a repository",
            path.display()
        )),
        std::io::ErrorKind::PermissionDenied => AppError::invalid(format!(
            "{} cannot be read by this user, so it cannot be registered as a repository",
            path.display()
        )),
        _ => AppError::io(path, source),
    })?;

    if !canonical.is_dir() {
        return Err(AppError::invalid(format!(
            "{} is a file; register the directory that contains it",
            canonical.display()
        )));
    }

    // Registering `/` or a home directory hands the model an unbounded reading
    // surface and makes every later limit meaningless. A user who genuinely
    // wants that can register a specific subdirectory instead.
    if is_too_broad(&canonical) {
        return Err(AppError::PolicyDenied {
            message: format!(
                "{} is too broad to register as a repository; register the project directory itself",
                canonical.display()
            ),
        });
    }

    Ok(canonical)
}

/// Whether a path is a filesystem or account root rather than a project.
fn is_too_broad(path: &Path) -> bool {
    if path.parent().is_none() {
        return true;
    }
    if let Some(home) = home_directory() {
        if path == home {
            return true;
        }
    }
    false
}

fn home_directory() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_becomes_an_absolute_canonical_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let nested = temp.path().join("project").join("src");
        std::fs::create_dir_all(&nested).expect("create");

        let root = canonical_root(&nested.join("..")).expect("canonicalize");
        assert!(root.is_absolute());
        assert!(!root.to_string_lossy().contains(".."));
        assert_eq!(
            root,
            std::fs::canonicalize(temp.path().join("project")).expect("expected")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_path_is_stored_as_its_target() {
        let temp = tempfile::tempdir().expect("temp dir");
        let real = temp.path().join("real-project");
        std::fs::create_dir_all(&real).expect("create");
        let link = temp.path().join("link-to-project");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        // Storing the link would let a later swap of the link point the
        // connector somewhere else entirely.
        assert_eq!(
            canonical_root(&link).expect("canonicalize"),
            std::fs::canonicalize(&real).expect("expected")
        );
    }

    #[test]
    fn a_missing_path_is_refused_with_an_explanation() {
        let temp = tempfile::tempdir().expect("temp dir");
        let error = canonical_root(&temp.path().join("absent")).expect_err("must refuse");
        assert_eq!(error.code(), "invalid_request");
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn a_file_is_refused_in_favour_of_its_directory() {
        let temp = tempfile::tempdir().expect("temp dir");
        let file = temp.path().join("README.md");
        std::fs::write(&file, "hello").expect("write");

        let error = canonical_root(&file).expect_err("must refuse");
        assert_eq!(error.code(), "invalid_request");
        assert!(error.to_string().contains("register the directory"));
    }

    #[test]
    fn the_filesystem_root_is_too_broad_to_register() {
        let error = canonical_root(Path::new("/")).expect_err("must refuse");
        assert_eq!(error.code(), "policy_denied");
    }

    #[test]
    fn the_policy_follows_the_configured_limits() {
        let limits = LimitSettings {
            max_file_bytes: 1234,
            max_results: 7,
            ..LimitSettings::default()
        };
        let policy = RepositoryPolicy::from_limits(&limits);
        assert_eq!(policy.max_file_bytes, 1234);
        assert_eq!(policy.max_results, 7);
        assert!(policy.respect_ignore_files);
        assert!(policy
            .secret_exclusions
            .iter()
            .any(|glob| glob.contains(".env")));
    }

    #[test]
    fn a_policy_round_trips_through_json() {
        let policy = RepositoryPolicy::default();
        let json = serde_json::to_string(&policy).expect("encode");
        assert_eq!(
            serde_json::from_str::<RepositoryPolicy>(&json).expect("decode"),
            policy
        );
    }

    #[test]
    fn a_stored_policy_missing_later_fields_still_loads() {
        // A repository registered by an earlier build must remain usable.
        let policy: RepositoryPolicy =
            serde_json::from_str(r#"{"max_file_bytes":1000,"max_results":10}"#).expect("decode");
        assert!(policy.respect_ignore_files);
        assert!(!policy.secret_exclusions.is_empty());
    }

    #[test]
    fn a_repository_reports_its_label_and_reachability() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("my-project");
        std::fs::create_dir_all(&root).expect("create");

        let repository = Repository {
            id: RepositoryId::new(),
            root: std::fs::canonicalize(&root).expect("canonical"),
            policy: RepositoryPolicy::default(),
            registered_at: chrono::Utc::now(),
        };
        assert_eq!(repository.label(), "my-project");
        assert!(repository.is_reachable());
        assert!(!repository.is_git_work_tree());

        std::fs::remove_dir_all(&root).expect("remove");
        assert!(!repository.is_reachable());
    }
}
