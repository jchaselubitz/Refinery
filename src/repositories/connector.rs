//! Bounded, read-only access to one registered repository.
//!
//! The connector is the only code an agent backend will use to inspect a
//! checkout. It owns the security boundary: callers supply relative paths,
//! every existing path is canonicalized below the registered root, and file
//! reads open then canonicalize again before reading. That second check turns
//! a symlink replacement between validation and open into a denial rather
//! than a read outside the registered repository.

use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
};

use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    WalkBuilder,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    domain::{CaseId, Outcome},
    error::{AppError, Result},
    repositories::{Repository, RepositoryPolicy},
    storage::Storage,
};

/// Input to `list_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFilesInput {
    /// Relative subtree to list. `None` means the registered root.
    pub path: Option<PathBuf>,
}

/// One file visible to the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Repository-relative, slash-preserving filesystem path.
    pub path: PathBuf,
    /// Measured size at traversal time.
    pub size_bytes: u64,
}

/// Result of `list_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFilesOutput {
    /// Files in deterministic lexical order.
    pub files: Vec<FileEntry>,
    /// Whether the policy stopped the traversal at `max_results`.
    pub truncated: bool,
}

/// Input to `read_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileInput {
    /// Required repository-relative path.
    pub path: PathBuf,
    /// One-based first line. Defaults to one.
    pub start_line: Option<usize>,
    /// Inclusive final line. Omit for the rest of the file, subject to limits.
    pub end_line: Option<usize>,
}

/// Result of `read_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileOutput {
    /// The requested UTF-8 text.
    pub content: String,
    /// First returned one-based line number.
    pub start_line: usize,
    /// Last returned one-based line number, or zero when no line was returned.
    pub end_line: usize,
    /// Whether the line-item policy truncated the requested range.
    pub truncated: bool,
}

/// Input to `search_text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchTextInput {
    /// Literal text to find. Regular expressions are intentionally not part of
    /// the first connector because their runtime and semantics are needlessly
    /// surprising at this trust boundary.
    pub query: String,
    /// Relative subtree to search. `None` means the registered root.
    pub path: Option<PathBuf>,
}

/// A text-search match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    /// Repository-relative file path.
    pub path: PathBuf,
    /// One-based source line.
    pub line: usize,
    /// The matching line, bounded by the file limit.
    pub text: String,
}

/// Result of `search_text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchTextOutput {
    /// Matches in stable path/line order.
    pub matches: Vec<SearchMatch>,
    /// Whether scanning stopped at the result-item limit.
    pub truncated: bool,
}

/// One Git porcelain status row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusEntry {
    /// Two-character Git index/worktree status.
    pub status: String,
    /// Repository-relative path as reported by Git.
    pub path: PathBuf,
}

/// Result of `git_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusOutput {
    /// Bounded porcelain entries.
    pub entries: Vec<GitStatusEntry>,
    /// Whether the item or byte limit cut the result short.
    pub truncated: bool,
}

/// Input to `git_diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiffInput {
    /// Optional existing relative path to restrict the diff to.
    pub path: Option<PathBuf>,
}

/// Result of `git_diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiffOutput {
    /// UTF-8 patch text, bounded by the registered file-byte policy.
    pub diff: String,
    /// Whether the byte limit cut the patch short.
    pub truncated: bool,
}

/// The repository-tool façade used by an agent backend for one case.
#[derive(Clone, Debug)]
pub struct RepositoryConnector {
    repository: Repository,
    storage: Storage,
}

impl RepositoryConnector {
    /// Bind a registered repository and its durable case-event store.
    pub fn new(repository: Repository, storage: Storage) -> Self {
        Self {
            repository,
            storage,
        }
    }

    /// The repository this connector is allowed to inspect.
    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    /// List regular files beneath a bounded, policy-filtered subtree.
    pub async fn list_files(
        &self,
        case_id: CaseId,
        input: ListFilesInput,
    ) -> Result<ListFilesOutput> {
        let audit_input = input.clone();
        self.invoke(case_id, "list_files", &audit_input, || {
            self.list_files_inner(input)
        })
        .await
    }

    /// Read a bounded UTF-8 line range from one approved file.
    pub async fn read_file(&self, case_id: CaseId, input: ReadFileInput) -> Result<ReadFileOutput> {
        let audit_input = input.clone();
        self.invoke(case_id, "read_file", &audit_input, || {
            self.read_file_inner(input)
        })
        .await
    }

    /// Find literal text in bounded, non-binary files.
    pub async fn search_text(
        &self,
        case_id: CaseId,
        input: SearchTextInput,
    ) -> Result<SearchTextOutput> {
        let audit_input = input.clone();
        self.invoke(case_id, "search_text", &audit_input, || {
            self.search_text_inner(input)
        })
        .await
    }

    /// Return a bounded, typed version of `git status --short`.
    pub async fn git_status(&self, case_id: CaseId) -> Result<GitStatusOutput> {
        self.invoke(case_id, "git_status", &(), || self.git_status_inner())
            .await
    }

    /// Return a bounded working-tree diff, optionally scoped to one file.
    pub async fn git_diff(&self, case_id: CaseId, input: GitDiffInput) -> Result<GitDiffOutput> {
        let audit_input = input.clone();
        self.invoke(case_id, "git_diff", &audit_input, || {
            self.git_diff_inner(input)
        })
        .await
    }

    async fn invoke<I: Serialize, O: ResultBytes>(
        &self,
        case_id: CaseId,
        tool: &str,
        input: &I,
        operation: impl FnOnce() -> Result<O>,
    ) -> Result<O> {
        let arguments_digest = digest(input)?;
        let result = operation();
        let (result_bytes, outcome) = match &result {
            Ok(output) => (output.result_bytes(), Outcome::Succeeded),
            Err(AppError::PolicyDenied { .. }) => (0, Outcome::Denied),
            Err(_) => (0, Outcome::Failed),
        };
        self.storage
            .record_repository_tool_invocation(
                case_id,
                tool,
                &arguments_digest,
                result_bytes,
                outcome,
            )
            .await?;
        result
    }

    fn list_files_inner(&self, input: ListFilesInput) -> Result<ListFilesOutput> {
        let start = self.resolve_subtree(input.path.as_deref())?;
        let secrets = SecretExclusions::new(&self.repository.root, &self.repository.policy)?;
        let mut builder = WalkBuilder::new(&start);
        builder
            .hidden(false)
            .follow_links(false)
            .sort_by_file_path(|left, right| left.cmp(right));
        builder
            .ignore(self.repository.policy.respect_ignore_files)
            .git_ignore(self.repository.policy.respect_ignore_files)
            .git_global(self.repository.policy.respect_ignore_files)
            .git_exclude(self.repository.policy.respect_ignore_files)
            .require_git(false);

        let mut files = Vec::new();
        let mut result_bytes = 0usize;
        let mut truncated = false;
        for entry in builder.build() {
            let entry = entry.map_err(|error| AppError::PolicyDenied {
                message: format!("could not traverse registered repository: {error}"),
            })?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let candidate = entry
                .path()
                .strip_prefix(&self.repository.root)
                .map_err(|_| outside_root())?;
            let canonical = self.resolve_existing(candidate)?;
            let relative = canonical
                .strip_prefix(&self.repository.root)
                .map_err(|_| outside_root())?
                .to_path_buf();
            if secrets.excludes(&relative, false) {
                continue;
            }
            let entry_bytes = relative.as_os_str().len();
            if files.len() == self.repository.policy.max_results
                || result_bytes.saturating_add(entry_bytes) > max_bytes(&self.repository.policy)
            {
                truncated = true;
                break;
            }
            let metadata =
                fs::metadata(&canonical).map_err(|error| AppError::io(&canonical, error))?;
            files.push(FileEntry {
                path: relative,
                size_bytes: metadata.len(),
            });
            result_bytes += entry_bytes;
        }
        Ok(ListFilesOutput { files, truncated })
    }

    fn read_file_inner(&self, input: ReadFileInput) -> Result<ReadFileOutput> {
        let canonical = self.resolve_existing(&input.path)?;
        let relative = self.relative(&canonical)?;
        if SecretExclusions::new(&self.repository.root, &self.repository.policy)?
            .excludes(&relative, false)
        {
            return Err(AppError::PolicyDenied {
                message: format!(
                    "{} is excluded by the repository secret policy",
                    relative.display()
                ),
            });
        }
        let bytes = self.open_text_file(&canonical)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| binary_file(&relative))?;
        let start = input.start_line.unwrap_or(1);
        let requested_end = input.end_line.unwrap_or(usize::MAX);
        if start == 0 || requested_end < start {
            return Err(AppError::invalid(
                "line ranges use positive one-based line numbers",
            ));
        }
        let lines: Vec<&str> = text.lines().collect();
        let first = start.saturating_sub(1);
        if first >= lines.len() {
            return Ok(ReadFileOutput {
                content: String::new(),
                start_line: start,
                end_line: 0,
                truncated: false,
            });
        }
        let last_exclusive = requested_end
            .min(lines.len())
            .min(first.saturating_add(self.repository.policy.max_results));
        let truncated = first.saturating_add(self.repository.policy.max_results)
            < requested_end.min(lines.len());
        Ok(ReadFileOutput {
            content: lines[first..last_exclusive].join("\n"),
            start_line: start,
            end_line: last_exclusive,
            truncated,
        })
    }

    fn search_text_inner(&self, input: SearchTextInput) -> Result<SearchTextOutput> {
        if input.query.is_empty() {
            return Err(AppError::invalid("search text cannot be empty"));
        }
        let start = self.resolve_subtree(input.path.as_deref())?;
        let secrets = SecretExclusions::new(&self.repository.root, &self.repository.policy)?;
        let mut builder = WalkBuilder::new(&start);
        builder
            .hidden(false)
            .follow_links(false)
            .sort_by_file_path(|left, right| left.cmp(right));
        builder
            .ignore(self.repository.policy.respect_ignore_files)
            .git_ignore(self.repository.policy.respect_ignore_files)
            .git_global(self.repository.policy.respect_ignore_files)
            .git_exclude(self.repository.policy.respect_ignore_files)
            .require_git(false);
        let mut matches = Vec::new();
        let mut result_bytes = 0usize;
        let mut truncated = false;
        for entry in builder.build() {
            let entry = entry.map_err(|error| AppError::PolicyDenied {
                message: format!("could not traverse registered repository: {error}"),
            })?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let candidate = entry
                .path()
                .strip_prefix(&self.repository.root)
                .map_err(|_| outside_root())?;
            let canonical = self.resolve_existing(candidate)?;
            let relative = self.relative(&canonical)?;
            if secrets.excludes(&relative, false) {
                continue;
            }
            let bytes = match self.open_text_file(&canonical) {
                Ok(bytes) => bytes,
                Err(AppError::PolicyDenied { .. }) => continue,
                Err(error) => return Err(error),
            };
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(_) => continue,
            };
            for (line, value) in text.lines().enumerate() {
                if value.contains(&input.query) {
                    let match_bytes = relative.as_os_str().len() + value.len();
                    if matches.len() == self.repository.policy.max_results
                        || result_bytes.saturating_add(match_bytes)
                            > max_bytes(&self.repository.policy)
                    {
                        truncated = true;
                        break;
                    }
                    matches.push(SearchMatch {
                        path: relative.clone(),
                        line: line + 1,
                        text: value.to_owned(),
                    });
                    result_bytes += match_bytes;
                }
            }
            if truncated {
                break;
            }
        }
        Ok(SearchTextOutput { matches, truncated })
    }

    fn git_status_inner(&self) -> Result<GitStatusOutput> {
        let output = self.run_git(["status", "--short", "--untracked-files=normal"])?;
        let text = String::from_utf8(output).map_err(|_| AppError::PolicyDenied {
            message: "Git returned non-text status output".into(),
        })?;
        let mut entries = Vec::new();
        let mut used_bytes = 0usize;
        let mut truncated = false;
        for line in text.lines() {
            let bytes = line.len() + 1;
            if entries.len() == self.repository.policy.max_results
                || used_bytes.saturating_add(bytes) > max_bytes(&self.repository.policy)
            {
                truncated = true;
                break;
            }
            if line.len() < 4 {
                continue;
            }
            entries.push(GitStatusEntry {
                status: line[..2].to_owned(),
                path: PathBuf::from(&line[3..]),
            });
            used_bytes += bytes;
        }
        Ok(GitStatusOutput { entries, truncated })
    }

    fn git_diff_inner(&self, input: GitDiffInput) -> Result<GitDiffOutput> {
        let mut arguments = vec![
            "diff".to_owned(),
            "--no-ext-diff".to_owned(),
            "--no-color".to_owned(),
        ];
        if let Some(path) = input.path {
            let canonical = self.resolve_existing(&path)?;
            let relative = self.relative(&canonical)?;
            if SecretExclusions::new(&self.repository.root, &self.repository.policy)?
                .excludes(&relative, false)
            {
                return Err(AppError::PolicyDenied {
                    message: format!(
                        "{} is excluded by the repository secret policy",
                        relative.display()
                    ),
                });
            }
            arguments.push("--".to_owned());
            arguments.push(relative.to_string_lossy().into_owned());
        }
        let output = self.run_git(arguments)?;
        let max = max_bytes(&self.repository.policy);
        let truncated = output.len() > max;
        let bounded = &output[..output.len().min(max)];
        let diff = String::from_utf8(bounded.to_vec()).map_err(|_| AppError::PolicyDenied {
            message: "Git returned non-text diff output".into(),
        })?;
        Ok(GitDiffOutput { diff, truncated })
    }

    fn run_git<I, S>(&self, arguments: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git")
            .current_dir(&self.repository.root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(arguments)
            .output()
            .map_err(|error| AppError::io(&self.repository.root, error))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(AppError::PolicyDenied {
                message: format!(
                    "Git command was refused or failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            })
        }
    }

    fn resolve_subtree(&self, path: Option<&Path>) -> Result<PathBuf> {
        match path {
            Some(path) => self.resolve_existing(path),
            None => Ok(self.repository.root.clone()),
        }
    }

    fn resolve_existing(&self, requested: &Path) -> Result<PathBuf> {
        relative_path(requested)?;
        let candidate = self.repository.root.join(requested);
        let canonical = fs::canonicalize(&candidate).map_err(|_| AppError::PolicyDenied {
            message: format!(
                "{} does not resolve inside the registered repository",
                requested.display()
            ),
        })?;
        if !canonical.starts_with(&self.repository.root) {
            return Err(outside_root());
        }
        Ok(canonical)
    }

    fn relative(&self, canonical: &Path) -> Result<PathBuf> {
        canonical
            .strip_prefix(&self.repository.root)
            .map(Path::to_path_buf)
            .map_err(|_| outside_root())
    }

    fn open_text_file(&self, canonical: &Path) -> Result<Vec<u8>> {
        let mut file = File::open(canonical).map_err(|error| AppError::io(canonical, error))?;
        // Do not read until after the second resolution. If a symlink was
        // changed after `resolve_existing` but before `open`, this observes its
        // new target and refuses it before repository data leaves the process.
        let verified = fs::canonicalize(canonical).map_err(|_| AppError::PolicyDenied {
            message: format!("{} changed while it was being opened", canonical.display()),
        })?;
        if !verified.starts_with(&self.repository.root) {
            return Err(outside_root());
        }
        let metadata = file
            .metadata()
            .map_err(|error| AppError::io(canonical, error))?;
        if metadata.len() > self.repository.policy.max_file_bytes {
            return Err(AppError::PolicyDenied {
                message: format!(
                    "{} is {} bytes, over the {} byte repository limit",
                    self.relative(&verified)?.display(),
                    metadata.len(),
                    self.repository.policy.max_file_bytes
                ),
            });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.by_ref()
            .take(self.repository.policy.max_file_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| AppError::io(canonical, error))?;
        if bytes.len() > max_bytes(&self.repository.policy) {
            return Err(AppError::PolicyDenied {
                message: format!(
                    "{} grew beyond the repository file-byte limit while being read",
                    self.relative(&verified)?.display()
                ),
            });
        }
        if bytes.contains(&0) {
            return Err(binary_file(&self.relative(&verified)?));
        }
        Ok(bytes)
    }
}

trait ResultBytes {
    fn result_bytes(&self) -> usize;
}
impl ResultBytes for ListFilesOutput {
    fn result_bytes(&self) -> usize {
        self.files
            .iter()
            .map(|item| item.path.as_os_str().len())
            .sum()
    }
}
impl ResultBytes for ReadFileOutput {
    fn result_bytes(&self) -> usize {
        self.content.len()
    }
}
impl ResultBytes for SearchTextOutput {
    fn result_bytes(&self) -> usize {
        self.matches
            .iter()
            .map(|item| item.path.as_os_str().len() + item.text.len())
            .sum()
    }
}
impl ResultBytes for GitStatusOutput {
    fn result_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|item| item.status.len() + item.path.as_os_str().len())
            .sum()
    }
}
impl ResultBytes for GitDiffOutput {
    fn result_bytes(&self) -> usize {
        self.diff.len()
    }
}

struct SecretExclusions {
    matcher: Gitignore,
}
impl SecretExclusions {
    fn new(root: &Path, policy: &RepositoryPolicy) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(root);
        for pattern in &policy.secret_exclusions {
            builder.add_line(None, pattern).map_err(|error| {
                AppError::config(format!(
                    "invalid repository secret exclusion {pattern:?}: {error}"
                ))
            })?;
        }
        let matcher = builder.build().map_err(|error| {
            AppError::config(format!("invalid repository secret exclusions: {error}"))
        })?;
        Ok(Self { matcher })
    }
    fn excludes(&self, relative: &Path, is_dir: bool) -> bool {
        self.matcher
            .matched_path_or_any_parents(relative, is_dir)
            .is_ignore()
    }
}

fn digest(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|error| AppError::Internal(error.into()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn relative_path(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(AppError::PolicyDenied {
            message:
                "repository tool paths must be relative and may not contain traversal components"
                    .into(),
        });
    }
    Ok(())
}
fn max_bytes(policy: &RepositoryPolicy) -> usize {
    policy.max_file_bytes.min(usize::MAX as u64) as usize
}
fn outside_root() -> AppError {
    AppError::PolicyDenied {
        message: "requested path resolves outside the registered repository".into(),
    }
}
fn binary_file(path: &Path) -> AppError {
    AppError::PolicyDenied {
        message: format!(
            "{} is binary and cannot be read by a text repository tool",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::process::Command;
    use tempfile::TempDir;

    use crate::{
        domain::{CaseEventPayload, RepositoryId},
        storage::CreateCaseResult,
    };

    async fn fixture(policy: RepositoryPolicy) -> (TempDir, RepositoryConnector, CaseId) {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("project");
        fs::create_dir(&root).expect("create project");
        let storage = Storage::open(temp.path().join("refinery.db"))
            .await
            .expect("open storage");
        let request = serde_json::from_str(include_str!(
            "../../tests/fixtures/contracts/refinement_request.json"
        ))
        .expect("fixture request");
        let case_id = match storage
            .create_case_idempotent(&request)
            .await
            .expect("create case")
        {
            CreateCaseResult::Created(id) => id,
            CreateCaseResult::Existing(_) => unreachable!("fresh storage"),
        };
        let repository = Repository {
            id: RepositoryId::new(),
            root: fs::canonicalize(root).expect("canonical root"),
            policy,
            registered_at: Utc::now(),
        };
        (temp, RepositoryConnector::new(repository, storage), case_id)
    }

    #[tokio::test]
    async fn traversal_and_absolute_paths_are_denied_and_audited() {
        let (temp, connector, case_id) = fixture(RepositoryPolicy::default()).await;
        fs::write(temp.path().join("outside.txt"), "private").expect("write outside");

        for path in [
            PathBuf::from("../outside.txt"),
            temp.path().join("outside.txt"),
        ] {
            let error = connector
                .read_file(
                    case_id,
                    ReadFileInput {
                        path,
                        start_line: None,
                        end_line: None,
                    },
                )
                .await
                .expect_err("path must be denied");
            assert_eq!(error.code(), "policy_denied");
        }

        let events = connector
            .storage
            .case_events(case_id)
            .await
            .expect("events");
        let denied = events
            .iter()
            .filter(|event| {
                matches!(
                    event.payload,
                    CaseEventPayload::RepositoryToolInvoked {
                        outcome: Outcome::Denied,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(denied, 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn escaping_and_swapped_symlinks_never_widen_the_root() {
        let (temp, connector, case_id) = fixture(RepositoryPolicy::default()).await;
        let root = connector.repository().root.clone();
        let safe = root.join("safe.txt");
        fs::write(&safe, "safe").expect("safe file");
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "outside").expect("outside file");
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).expect("escaping link");

        let error = connector
            .read_file(
                case_id,
                ReadFileInput {
                    path: PathBuf::from("link.txt"),
                    start_line: None,
                    end_line: None,
                },
            )
            .await
            .expect_err("escaping link must be denied");
        assert_eq!(error.code(), "policy_denied");

        // Resolve an in-root link, then replace that link before the read. The
        // connector opens the canonical in-root object rather than following
        // the mutable link again, so the replacement cannot redirect the read.
        fs::remove_file(&link).expect("remove link");
        std::os::unix::fs::symlink(&safe, &link).expect("safe link");
        let resolved = connector
            .resolve_existing(Path::new("link.txt"))
            .expect("resolve safe link");
        fs::remove_file(&link).expect("swap link");
        std::os::unix::fs::symlink(&outside, &link).expect("replace with outside");
        assert_eq!(
            connector
                .open_text_file(&resolved)
                .expect("read resolved file"),
            b"safe"
        );
    }

    #[tokio::test]
    async fn files_are_text_only_and_all_text_results_are_bounded() {
        let policy = RepositoryPolicy {
            max_file_bytes: 13,
            max_results: 2,
            ..RepositoryPolicy::default()
        };
        let (_temp, connector, case_id) = fixture(policy).await;
        let root = connector.repository().root.clone();
        fs::write(root.join("large.txt"), "this is too large").expect("large file");
        fs::write(root.join("binary.bin"), [b'a', 0, b'b']).expect("binary file");
        fs::write(root.join("lines.txt"), "one\ntwo\nthree").expect("lines");

        for path in ["large.txt", "binary.bin"] {
            let error = connector
                .read_file(
                    case_id,
                    ReadFileInput {
                        path: PathBuf::from(path),
                        start_line: None,
                        end_line: None,
                    },
                )
                .await
                .expect_err("unsafe text read must fail");
            assert_eq!(error.code(), "policy_denied");
        }
        let read = connector
            .read_file(
                case_id,
                ReadFileInput {
                    path: PathBuf::from("lines.txt"),
                    start_line: Some(1),
                    end_line: None,
                },
            )
            .await
            .expect("read lines");
        assert_eq!(read.content, "one\ntwo");
        assert!(read.truncated);

        let search = connector
            .search_text(
                case_id,
                SearchTextInput {
                    query: "a".into(),
                    path: None,
                },
            )
            .await
            .expect("search text");
        assert!(search
            .matches
            .iter()
            .all(|item| item.path != Path::new("binary.bin")));
    }

    #[tokio::test]
    async fn ignores_and_secret_globs_filter_list_and_search() {
        let (_temp, connector, case_id) = fixture(RepositoryPolicy::default()).await;
        let root = connector.repository().root.clone();
        fs::write(root.join(".gitignore"), "ignored.txt\n").expect("ignore file");
        fs::write(root.join("ignored.txt"), "needle").expect("ignored");
        fs::write(root.join("visible.txt"), "needle").expect("visible");
        fs::write(root.join(".env"), "TOKEN=needle").expect("secret");

        let listed = connector
            .list_files(case_id, ListFilesInput { path: None })
            .await
            .expect("list files");
        let names: Vec<_> = listed
            .files
            .iter()
            .map(|entry| entry.path.as_path())
            .collect();
        assert!(names.contains(&Path::new("visible.txt")));
        assert!(!names.contains(&Path::new("ignored.txt")));
        assert!(!names.contains(&Path::new(".env")));

        let found = connector
            .search_text(
                case_id,
                SearchTextInput {
                    query: "needle".into(),
                    path: None,
                },
            )
            .await
            .expect("search text");
        assert_eq!(found.matches.len(), 1);
        assert_eq!(found.matches[0].path, PathBuf::from("visible.txt"));
    }

    #[tokio::test]
    async fn git_tools_return_bounded_typed_results_and_are_audited() {
        let policy = RepositoryPolicy {
            max_file_bytes: 1024,
            max_results: 1,
            ..RepositoryPolicy::default()
        };
        let (_temp, connector, case_id) = fixture(policy).await;
        let root = connector.repository().root.clone();
        run_git(&root, ["init"]);
        fs::write(root.join("tracked.txt"), "before\n").expect("tracked");
        run_git(&root, ["add", "tracked.txt"]);
        fs::write(root.join("tracked.txt"), "after\n").expect("modify");
        fs::write(root.join("untracked.txt"), "untracked\n").expect("untracked");

        let status = connector.git_status(case_id).await.expect("status");
        assert!(!status.entries.is_empty());
        assert!(status.entries.len() <= 1);
        let diff = connector
            .git_diff(
                case_id,
                GitDiffInput {
                    path: Some(PathBuf::from("tracked.txt")),
                },
            )
            .await
            .expect("diff");
        assert!(diff.diff.contains("-before"));
        assert!(diff.diff.contains("+after"));

        let events = connector
            .storage
            .case_events(case_id)
            .await
            .expect("events");
        assert!(events.iter().any(|event| matches!(event.payload, CaseEventPayload::RepositoryToolInvoked { ref tool, outcome: Outcome::Succeeded, .. } if tool == "git_status")));
        assert!(events.iter().any(|event| matches!(event.payload, CaseEventPayload::RepositoryToolInvoked { ref tool, outcome: Outcome::Succeeded, .. } if tool == "git_diff")));
    }

    fn run_git<const N: usize>(root: &Path, arguments: [&str; N]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(arguments)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
