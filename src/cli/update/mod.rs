//! `refinery update` — replace this install with the newest published release.
//!
//! Refinery ships as one signed binary with its interface compiled in, so an
//! update is a single atomic replacement. That is the whole feature, and the
//! risk is entirely in the details: the archive must be the one the release
//! published, the swap must not leave a half-written binary where a working one
//! was, and a copy that somebody else owns — a Homebrew cellar, a Nix store
//! path — must be refused rather than quietly diverged from its package.
//!
//! Four checks stand between the network and the installed binary:
//!
//! 1. the archive's SHA-256 must match the release's own `checksums.txt`;
//! 2. its payload manifest must name this version, target, and binary;
//! 3. when the running binary carries a Developer ID signature, the downloaded
//!    one must carry a valid signature from the same team;
//! 4. the replacement is staged beside the target and moved into place with
//!    `rename`, so the installed path is either the old binary or the new one.
//!
//! Unpacking is `ditto`/`tar`, both part of the platforms Refinery publishes
//! for. Downloading uses the HTTP client the daemon already links, so an update
//! needs nothing on the machine that a plain `curl | sh` install did not.

pub mod release;

use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, Result, RetryClass};
use release::{host_target, Release, Version, DEFAULT_RELEASE_REPO};

/// The one binary a release archive installs.
const PAYLOAD_BINARY: &str = "refinery";
/// The manifest that names what an archive is supposed to contain.
const PAYLOAD_MANIFEST_NAME: &str = "refinery-payload.json";

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PayloadManifest {
    format_version: u32,
    version: String,
    target: String,
    binaries: Vec<String>,
}

/// What an update run was asked to do.
#[derive(Debug, Clone)]
pub struct UpdateOptions {
    /// Version this binary reports as its own.
    pub current_version: String,
    /// Report what is available without installing it.
    pub check_only: bool,
    /// Install the published release even when it is not newer.
    ///
    /// The repair path: a truncated or hand-edited binary reports whatever
    /// version it was built as, so "already current" is not the same as
    /// "already correct".
    pub force: bool,
    /// Binary to replace. Defaults to the running executable.
    pub executable: Option<PathBuf>,
    /// Release target triple to install. Defaults to this machine's.
    pub target: Option<String>,
}

impl UpdateOptions {
    /// Options for the running binary on this machine.
    pub fn new(current_version: impl Into<String>) -> Self {
        Self {
            current_version: current_version.into(),
            check_only: false,
            force: false,
            executable: None,
            target: None,
        }
    }
}

/// What `update` did.
///
/// `Current` and `Available` are the two outcomes of a check; a run that
/// installs reports `Installed` with the version now on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    /// The installed version is the published one.
    Current,
    /// A newer release exists and was not installed.
    Available,
    /// A newer release was installed.
    Installed,
}

impl UpdateStatus {
    /// The wire spelling used by `--json`.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Available => "available",
            Self::Installed => "installed",
        }
    }
}

/// `refinery update --json`.
///
/// One shape for all three outcomes so a caller can branch on `status` alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateReport {
    /// `current` | `available` | `installed`.
    pub status: String,
    /// Version that was running when the check was made.
    pub current_version: String,
    /// Newest published version, when the release feed was readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    /// Release page for the newest published version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    /// Binary that was replaced, present only for `installed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<PathBuf>,
}

/// Where release metadata and archives come from.
///
/// A trait so the install path can be exercised without a network: the tests
/// serve a release document and archives from a directory, and everything
/// downstream of the fetch — digest verification, unpacking, the atomic swap —
/// runs for real.
#[async_trait]
pub trait ReleaseSource: Send + Sync {
    /// The newest published release, as the GitHub releases API renders it.
    async fn latest_release(&self) -> Result<String>;
    /// Download `url` to `destination`.
    async fn download(&self, url: &str, destination: &Path) -> Result<()>;
    /// Fetch a small text file, such as `checksums.txt`.
    async fn fetch_text(&self, url: &str) -> Result<String>;
}

/// The published GitHub releases of one repository.
#[derive(Debug, Clone)]
pub struct GitHubReleases {
    repo: String,
}

impl Default for GitHubReleases {
    fn default() -> Self {
        Self {
            repo: DEFAULT_RELEASE_REPO.to_owned(),
        }
    }
}

impl GitHubReleases {
    async fn get(&self, url: &str, accept: Option<&str>) -> Result<reqwest::Response> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("refinery/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|source| upstream(format!("could not build an HTTP client: {source}")))?;
        let mut request = client.get(url);
        if let Some(accept) = accept {
            request = request.header(reqwest::header::ACCEPT, accept);
        }
        let response = request
            .send()
            .await
            .map_err(|source| upstream(format!("could not reach {url}: {source}")))?;
        if !response.status().is_success() {
            return Err(upstream(format!(
                "{url} answered {}",
                response.status().as_u16()
            )));
        }
        Ok(response)
    }
}

fn upstream(message: String) -> AppError {
    AppError::Upstream {
        service: "github",
        message,
        retry: RetryClass::Retryable,
    }
}

#[async_trait]
impl ReleaseSource for GitHubReleases {
    async fn latest_release(&self) -> Result<String> {
        let url = format!("https://api.github.com/repos/{}/releases/latest", self.repo);
        let response = self
            .get(&url, Some("application/vnd.github+json"))
            .await
            .map_err(|error| match error {
                AppError::Upstream { message, .. } => upstream(format!(
                    "could not read the Refinery release feed: {message}"
                )),
                other => other,
            })?;
        response
            .text()
            .await
            .map_err(|source| upstream(format!("the release feed was unreadable: {source}")))
    }

    async fn download(&self, url: &str, destination: &Path) -> Result<()> {
        let response = self.get(url, None).await?;
        let body = response
            .bytes()
            .await
            .map_err(|source| upstream(format!("could not download {url}: {source}")))?;
        std::fs::write(destination, &body).map_err(|source| AppError::io(destination, source))
    }

    async fn fetch_text(&self, url: &str) -> Result<String> {
        let response = self.get(url, None).await?;
        response
            .text()
            .await
            .map_err(|source| upstream(format!("{url} was not valid UTF-8: {source}")))
    }
}

/// Whether `refinery update` could replace this install in place.
///
/// Reported by `refinery doctor` so a user on a package-managed copy is told to
/// update it the way the package expects rather than being offered a command
/// that will refuse.
pub fn can_self_update() -> bool {
    if host_target().is_err() {
        return false;
    }
    match std::env::current_exe() {
        Ok(path) => {
            let resolved = std::fs::canonicalize(&path).unwrap_or(path);
            refuse_managed_location(&resolved).is_ok()
        }
        Err(_) => false,
    }
}

/// Check for, and unless asked not to install, the newest published release.
pub async fn update(options: UpdateOptions) -> Result<UpdateReport> {
    update_from(options, &GitHubReleases::default()).await
}

/// [`update`], against a caller-supplied release source.
pub async fn update_from(
    options: UpdateOptions,
    source: &dyn ReleaseSource,
) -> Result<UpdateReport> {
    let current = Version::parse(&options.current_version)?;
    let release = Release::parse(&source.latest_release().await?)?;

    let newer = release.version > current;
    if !newer && !options.force {
        return Ok(UpdateReport {
            status: UpdateStatus::Current.as_wire().to_owned(),
            current_version: current.to_string(),
            latest_version: Some(release.version.to_string()),
            release_url: Some(release.url),
            installed_path: None,
        });
    }

    if options.check_only {
        return Ok(UpdateReport {
            status: UpdateStatus::Available.as_wire().to_owned(),
            current_version: current.to_string(),
            latest_version: Some(release.version.to_string()),
            release_url: Some(release.url),
            installed_path: None,
        });
    }

    let triple = match &options.target {
        Some(triple) => triple.clone(),
        None => host_target()?.to_owned(),
    };
    let target = match options.executable {
        Some(path) => path,
        None => std::env::current_exe()
            .map_err(|source| AppError::io("the running executable", source))?,
    };
    // A symlinked install (Homebrew's `bin/refinery`, a `~/.local/bin` shim)
    // must have the real file replaced, not the link: renaming over the link
    // would break the package that owns it.
    let target = std::fs::canonicalize(&target).map_err(|source| AppError::io(&target, source))?;
    refuse_managed_location(&target)?;

    let installed = install(source, &release, &target, &triple).await?;

    Ok(UpdateReport {
        status: UpdateStatus::Installed.as_wire().to_owned(),
        current_version: current.to_string(),
        latest_version: Some(release.version.to_string()),
        release_url: Some(release.url),
        installed_path: Some(installed),
    })
}

/// Refuse to replace a binary that something else is responsible for.
///
/// A Homebrew cellar copy is owned by a formula, and silently diverging from it
/// turns the next `brew upgrade` into a surprise. A Nix store path is read-only
/// on purpose. A copy inside an application bundle is covered by that bundle's
/// signature, which replacing it would break.
pub(crate) fn refuse_managed_location(target: &Path) -> Result<()> {
    let display = target.display();
    if target
        .components()
        .any(|component| component.as_os_str().to_string_lossy().ends_with(".app"))
    {
        return Err(AppError::NeedsUser {
            message: format!(
                "{display} is bundled inside an application; replacing it would break the \
                 bundle's signature. Update the application instead."
            ),
        });
    }
    if target.starts_with("/opt/homebrew/Cellar") || target.starts_with("/usr/local/Cellar") {
        return Err(AppError::NeedsUser {
            message: format!(
                "{display} is managed by Homebrew; run `brew upgrade refinery` instead."
            ),
        });
    }
    if target.starts_with("/nix/store") {
        return Err(AppError::NeedsUser {
            message: format!(
                "{display} is in the Nix store, which is read-only; update it through Nix instead."
            ),
        });
    }
    Ok(())
}

/// Download, verify, and swap in the release build for this machine.
async fn install(
    source: &dyn ReleaseSource,
    release: &Release,
    target: &Path,
    triple: &str,
) -> Result<PathBuf> {
    let archive = release
        .cli_archive(triple)
        .ok_or_else(|| AppError::NeedsUser {
            message: format!(
                "release {} has no {triple} archive; download one from {} by hand",
                release.tag, release.url
            ),
        })?;

    let parent = target.parent().ok_or_else(|| {
        AppError::invalid(format!("{} has no parent directory", target.display()))
    })?;
    // Staging inside the destination directory keeps the final step a
    // same-filesystem `rename`, which is the only way the swap is atomic, and
    // it fails here — before anything is downloaded — when the directory is not
    // writable.
    let staging = Staging::create(parent, target)?;

    let archive_path = staging.path().join(&archive.name);
    source.download(&archive.url, &archive_path).await?;
    verify_digest(source, release, &archive.name, &archive_path).await?;

    let unpacked = staging.path().join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|source| AppError::io(&unpacked, source))?;
    unpack(&archive_path, &unpacked)?;

    verify_payload_manifest(&unpacked, release, triple)?;

    let replacement = unpacked.join(PAYLOAD_BINARY);
    if !replacement.is_file() {
        return Err(AppError::invalid(format!(
            "{} did not contain a `{PAYLOAD_BINARY}` binary; nothing was installed",
            archive.name
        )));
    }
    verify_signature(target, &replacement)?;

    // Match the mode of what is being replaced, so an install that was
    // deliberately group-readable-only stays that way; fall back to the mode
    // `install -m 755` gives a fresh copy.
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(target)
            .map(|meta| meta.permissions().mode() & 0o7777)
            .unwrap_or(0o755);
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(mode))
            .map_err(|source| AppError::io(&replacement, source))?;
    }

    if !reports_version(&replacement, &release.version.to_string()) {
        return Err(AppError::invalid(format!(
            "{} contains a `{PAYLOAD_BINARY}` that does not report version {}; nothing was installed",
            archive.name, release.version
        )));
    }

    std::fs::rename(&replacement, target).map_err(|source| AppError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    Ok(target.to_path_buf())
}

/// Whether a staged binary is the build the release claims it is.
///
/// Clap prints `refinery <version>` for `--version`, so this is both a
/// liveness check — the binary runs on this machine — and a mixed-archive
/// check.
fn reports_version(binary: &Path, version: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim()
                    == format!("{PAYLOAD_BINARY} {version}")
        })
}

fn verify_payload_manifest(unpacked: &Path, release: &Release, triple: &str) -> Result<()> {
    let path = unpacked.join(PAYLOAD_MANIFEST_NAME);
    let bytes = std::fs::read(&path).map_err(|source| AppError::io(&path, source))?;
    let actual: PayloadManifest = serde_json::from_slice(&bytes).map_err(|source| {
        AppError::invalid(format!("{PAYLOAD_MANIFEST_NAME} is invalid: {source}"))
    })?;
    let expected = PayloadManifest {
        format_version: 1,
        version: release.version.to_string(),
        target: triple.to_owned(),
        binaries: vec![PAYLOAD_BINARY.to_owned()],
    };
    if actual != expected {
        return Err(AppError::invalid(format!(
            "{PAYLOAD_MANIFEST_NAME} does not describe the requested {} {triple} payload; \
             nothing was installed",
            release.version
        )));
    }
    Ok(())
}

/// Hash the downloaded archive and compare it with the release's manifest.
async fn verify_digest(
    source: &dyn ReleaseSource,
    release: &Release,
    asset_name: &str,
    archive: &Path,
) -> Result<()> {
    let manifest = release.checksums().ok_or_else(|| {
        AppError::invalid(format!(
            "release {} publishes no checksums.txt, so the download cannot be verified",
            release.tag
        ))
    })?;
    let document = source.fetch_text(&manifest.url).await?;
    let expected = release::expected_digest(&document, asset_name).ok_or_else(|| {
        AppError::invalid(format!(
            "checksums.txt for {} does not list {asset_name}",
            release.tag
        ))
    })?;
    let actual = hex_digest_of_file(archive)?;
    if actual != expected {
        return Err(AppError::invalid(format!(
            "{asset_name} does not match the checksum {expected} published for it \
             (got {actual}); nothing was installed"
        )));
    }
    Ok(())
}

/// The SHA-256 of a file, streamed so a large archive is not held in memory.
fn hex_digest_of_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(|source| AppError::io(path, source))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| AppError::io(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Unpack a release archive, whichever of the two published formats it is.
///
/// `ditto` rather than `unzip` for ZIPs on macOS: it is what created the
/// archive on the release machine, and it preserves the extended attributes a
/// signed binary carries.
fn unpack(archive: &Path, into: &Path) -> Result<()> {
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    let (program, arguments): (&str, Vec<&std::ffi::OsStr>) =
        if name.ends_with(".zip") && cfg!(target_os = "macos") {
            (
                "ditto",
                vec![
                    "-x".as_ref(),
                    "-k".as_ref(),
                    archive.as_os_str(),
                    into.as_os_str(),
                ],
            )
        } else if name.ends_with(".zip") {
            (
                "unzip",
                vec![
                    "-q".as_ref(),
                    archive.as_os_str(),
                    "-d".as_ref(),
                    into.as_os_str(),
                ],
            )
        } else {
            (
                "tar",
                vec![
                    "-xzf".as_ref(),
                    archive.as_os_str(),
                    "-C".as_ref(),
                    into.as_os_str(),
                ],
            )
        };
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| AppError::io(program, source))?;
    if !output.status.success() {
        return Err(AppError::invalid(format!(
            "could not unpack {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Require the replacement to be signed by whoever signed the running binary.
///
/// Conditional on the current binary being signed at all, so a `cargo build`
/// copy can still update itself, but a Developer ID install can never be
/// replaced by something signed by somebody else — the check the checksum
/// cannot make, because a manifest and the archive it describes come from the
/// same place.
fn verify_signature(current: &Path, replacement: &Path) -> Result<()> {
    let Some(expected_team) = signing_team(current) else {
        return Ok(());
    };
    match signing_team(replacement) {
        Some(team) if team == expected_team => Ok(()),
        Some(team) => Err(AppError::invalid(format!(
            "the downloaded binary is signed by team {team}, but this install is signed by \
             {expected_team}; nothing was installed"
        ))),
        None => Err(AppError::invalid(format!(
            "the downloaded binary is not validly signed, but this install is (team \
             {expected_team}); nothing was installed"
        ))),
    }
}

/// The Team ID of a validly signed binary, or `None` when it is unsigned,
/// ad-hoc signed, or when `codesign` is unavailable.
fn signing_team(path: &Path) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let verified = Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !verified.status.success() {
        return None;
    }
    let described = Command::new("codesign")
        .args(["--display", "--verbose=4"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    // `codesign --display` writes its description to stderr.
    let text = String::from_utf8_lossy(&described.stderr);
    text.lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_owned)
}

/// A scratch directory beside the binary being replaced, removed on drop.
struct Staging {
    path: PathBuf,
}

impl Staging {
    fn create(parent: &Path, target: &Path) -> Result<Self> {
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| PAYLOAD_BINARY.to_owned());
        let path = parent.join(format!(".{name}-update-{}", std::process::id()));
        if let Err(source) = std::fs::remove_dir_all(&path) {
            if source.kind() != std::io::ErrorKind::NotFound {
                return Err(AppError::io(&path, source));
            }
        }
        std::fs::create_dir_all(&path).map_err(|source| AppError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests;
