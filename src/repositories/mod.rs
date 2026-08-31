//! The read-only repository connector.
//!
//! Repositories are registered by canonical path with a stored access policy.
//! The five tools — `list_files`, `read_file`, `search_text`, `git_status`,
//! `git_diff` — enforce byte and item limits inside the connector, reject
//! binary reads on text tools, and canonicalize and prefix-check every
//! requested path after symlink resolution, verifying again after opening so a
//! symlink swapped between the check and the read cannot escape the root.
//! Every invocation emits a per-case audit event. Filled in M4.

pub mod connector;
pub mod registry;

pub use connector::{
    FileEntry, GitDiffInput, GitDiffOutput, GitStatusEntry, GitStatusOutput, ListFilesInput,
    ListFilesOutput, ReadFileInput, ReadFileOutput, RepositoryConnector, SearchMatch,
    SearchTextInput, SearchTextOutput,
};
pub use registry::{canonical_root, Repository, RepositoryPolicy};
