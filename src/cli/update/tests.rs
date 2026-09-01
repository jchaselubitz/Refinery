//! Update tests.
//!
//! The release feed and its assets are served from a directory, so the
//! download, digest verification, unpacking, manifest check, and atomic swap
//! all run for real without a network and without touching a published
//! release.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use super::*;

/// The machine the fixture releases claim to be built for. Pinned rather than
/// probed so the suite runs the same way on the Linux CI leg, where there is no
/// published build for the host.
const FIXTURE_TARGET: &str = "aarch64-apple-darwin";

struct FakeSource {
    document: String,
    files: HashMap<String, Vec<u8>>,
    fetched: Mutex<Vec<String>>,
}

#[async_trait]
impl ReleaseSource for FakeSource {
    async fn latest_release(&self) -> Result<String> {
        Ok(self.document.clone())
    }

    async fn download(&self, url: &str, destination: &Path) -> Result<()> {
        self.fetched.lock().unwrap().push(url.to_owned());
        let body = self
            .files
            .get(url)
            .ok_or_else(|| AppError::invalid(format!("no such asset: {url}")))?;
        std::fs::write(destination, body).map_err(|source| AppError::io(destination, source))
    }

    async fn fetch_text(&self, url: &str) -> Result<String> {
        let body = self
            .files
            .get(url)
            .ok_or_else(|| AppError::invalid(format!("no such asset: {url}")))?;
        Ok(String::from_utf8(body.clone()).expect("utf-8 fixture"))
    }
}

/// A stand-in for the released binary: a script that answers `--version` the
/// way the real one does, which is all the install path asks of it.
fn fake_binary(version: &str) -> String {
    format!("#!/bin/sh\nprintf 'refinery {version}\\n'\n")
}

/// Build a `.tar.gz` release archive holding `files`, and return its bytes.
fn archive(directory: &Path, files: &[(&str, String, bool)]) -> Vec<u8> {
    let staging = directory.join("stage");
    std::fs::create_dir_all(&staging).expect("staging");
    for (name, body, executable) in files {
        let path = staging.join(name);
        std::fs::write(&path, body).expect("write member");
        if *executable {
            #[cfg(unix)]
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod member");
        }
    }
    let archive_path = directory.join("archive.tar.gz");
    let names: Vec<&str> = files.iter().map(|(name, _, _)| *name).collect();
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging)
        .args(&names)
        .status()
        .expect("run tar");
    assert!(status.success(), "tar failed");
    let bytes = std::fs::read(&archive_path).expect("read archive");
    std::fs::remove_dir_all(&staging).expect("clear staging");
    std::fs::remove_file(&archive_path).expect("clear archive");
    bytes
}

fn manifest_json(version: &str, target: &str) -> String {
    serde_json::to_string(&PayloadManifest {
        format_version: 1,
        version: version.to_owned(),
        target: target.to_owned(),
        binaries: vec![PAYLOAD_BINARY.to_owned()],
    })
    .expect("serialize manifest")
}

/// A release of `version` whose archive holds `manifest` and a binary that
/// reports `binary_version`.
struct Fixture {
    source: FakeSource,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    target: PathBuf,
}

fn fixture(version: &str, manifest: String, binary_version: &str, digest_matches: bool) -> Fixture {
    let root = tempfile::tempdir().expect("temp dir");
    let archive_name = format!("refinery-{version}-{FIXTURE_TARGET}.tar.gz");
    let bytes = archive(
        root.path(),
        &[
            (PAYLOAD_BINARY, fake_binary(binary_version), true),
            (PAYLOAD_MANIFEST_NAME, manifest, false),
        ],
    );

    let archive_url = format!("https://example.invalid/{archive_name}");
    let checksums_url = "https://example.invalid/checksums.txt".to_owned();
    let digest = {
        let path = root.path().join("digest-input");
        std::fs::write(&path, &bytes).expect("write archive");
        let digest = hex_digest_of_file(&path).expect("digest");
        std::fs::remove_file(&path).expect("clear");
        if digest_matches {
            digest
        } else {
            "0".repeat(64)
        }
    };

    let document = format!(
        r#"{{"tag_name":"v{version}","html_url":"https://example.invalid/tag","assets":[
            {{"name":"checksums.txt","browser_download_url":"{checksums_url}"}},
            {{"name":"{archive_name}","browser_download_url":"{archive_url}"}}
        ]}}"#
    );

    let mut files = HashMap::new();
    files.insert(archive_url, bytes);
    files.insert(
        checksums_url,
        format!("{digest}  dist/{archive_name}\n").into_bytes(),
    );

    // The binary being replaced: distinguishable content, so a refused install
    // can be shown to have left it alone.
    let target = root.path().join("bin").join(PAYLOAD_BINARY);
    std::fs::create_dir_all(target.parent().unwrap()).expect("bin dir");
    std::fs::write(&target, fake_binary("0.1.0")).expect("write target");
    #[cfg(unix)]
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let target = std::fs::canonicalize(&target).expect("canonical target");

    Fixture {
        source: FakeSource {
            document,
            files,
            fetched: Mutex::new(Vec::new()),
        },
        root,
        target,
    }
}

fn options_for(target: &Path, current_version: &str) -> UpdateOptions {
    UpdateOptions {
        executable: Some(target.to_path_buf()),
        target: Some(FIXTURE_TARGET.to_owned()),
        ..UpdateOptions::new(current_version)
    }
}

#[tokio::test]
async fn an_install_that_is_already_the_published_version_downloads_nothing() {
    let fixture = fixture(
        "0.2.0",
        manifest_json("0.2.0", FIXTURE_TARGET),
        "0.2.0",
        true,
    );
    let report = update_from(options_for(&fixture.target, "0.2.0"), &fixture.source)
        .await
        .expect("update");

    assert_eq!(report.status, "current");
    assert_eq!(report.latest_version.as_deref(), Some("0.2.0"));
    assert!(fixture.source.fetched.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_check_reports_a_newer_release_without_installing_it() {
    let fixture = fixture(
        "0.3.0",
        manifest_json("0.3.0", FIXTURE_TARGET),
        "0.3.0",
        true,
    );
    let mut options = options_for(&fixture.target, "0.2.0");
    options.check_only = true;
    let report = update_from(options, &fixture.source).await.expect("update");

    assert_eq!(report.status, "available");
    assert_eq!(report.latest_version.as_deref(), Some("0.3.0"));
    assert!(report.installed_path.is_none());
    assert!(fixture.source.fetched.lock().unwrap().is_empty());
    assert!(std::fs::read_to_string(&fixture.target)
        .expect("read target")
        .contains("0.1.0"));
}

#[tokio::test]
async fn a_verified_release_replaces_the_binary_in_place() {
    let fixture = fixture(
        "0.3.0",
        manifest_json("0.3.0", FIXTURE_TARGET),
        "0.3.0",
        true,
    );
    let report = update_from(options_for(&fixture.target, "0.2.0"), &fixture.source)
        .await
        .expect("update");

    assert_eq!(report.status, "installed");
    assert_eq!(
        report.installed_path.as_deref(),
        Some(fixture.target.as_path())
    );
    assert!(std::fs::read_to_string(&fixture.target)
        .expect("read target")
        .contains("0.3.0"));
    // Nothing is left beside the binary once the staging directory drops.
    let leftovers: Vec<_> = std::fs::read_dir(fixture.target.parent().unwrap())
        .expect("read bin dir")
        .map(|entry| entry.expect("entry").file_name())
        .filter(|name| name != PAYLOAD_BINARY)
        .collect();
    assert!(leftovers.is_empty(), "left {leftovers:?} behind");
}

#[tokio::test]
async fn an_archive_that_does_not_match_its_published_checksum_installs_nothing() {
    let fixture = fixture(
        "0.3.0",
        manifest_json("0.3.0", FIXTURE_TARGET),
        "0.3.0",
        false,
    );
    let error = update_from(options_for(&fixture.target, "0.2.0"), &fixture.source)
        .await
        .expect_err("must refuse");

    assert!(error.detail().contains("does not match the checksum"));
    assert!(std::fs::read_to_string(&fixture.target)
        .expect("read target")
        .contains("0.1.0"));
}

#[tokio::test]
async fn a_manifest_that_names_another_target_installs_nothing() {
    let fixture = fixture(
        "0.3.0",
        manifest_json("0.3.0", "x86_64-apple-darwin"),
        "0.3.0",
        true,
    );
    let error = update_from(options_for(&fixture.target, "0.2.0"), &fixture.source)
        .await
        .expect_err("must refuse");

    assert!(error.detail().contains(PAYLOAD_MANIFEST_NAME));
    assert!(std::fs::read_to_string(&fixture.target)
        .expect("read target")
        .contains("0.1.0"));
}

#[tokio::test]
async fn a_mixed_version_archive_installs_nothing() {
    let fixture = fixture(
        "0.3.0",
        manifest_json("0.3.0", FIXTURE_TARGET),
        "0.2.9",
        true,
    );
    let error = update_from(options_for(&fixture.target, "0.2.0"), &fixture.source)
        .await
        .expect_err("must refuse");

    assert!(error.detail().contains("does not report version"));
    assert!(std::fs::read_to_string(&fixture.target)
        .expect("read target")
        .contains("0.1.0"));
}

#[tokio::test]
async fn a_release_without_a_build_for_this_machine_is_an_error() {
    let fixture = fixture(
        "0.3.0",
        manifest_json("0.3.0", FIXTURE_TARGET),
        "0.3.0",
        true,
    );
    let mut options = options_for(&fixture.target, "0.2.0");
    options.target = Some("aarch64-unknown-linux-gnu".to_owned());
    let error = update_from(options, &fixture.source)
        .await
        .expect_err("must refuse");

    assert!(error.detail().contains("aarch64-unknown-linux-gnu"));
}

#[tokio::test]
async fn force_reinstalls_the_published_version_over_an_identical_one() {
    let fixture = fixture(
        "0.3.0",
        manifest_json("0.3.0", FIXTURE_TARGET),
        "0.3.0",
        true,
    );
    let mut options = options_for(&fixture.target, "0.3.0");
    options.force = true;
    let report = update_from(options, &fixture.source).await.expect("update");

    assert_eq!(report.status, "installed");
    assert!(std::fs::read_to_string(&fixture.target)
        .expect("read target")
        .contains("0.3.0"));
}

#[test]
fn a_package_managed_copy_is_refused_before_anything_is_downloaded() {
    for path in [
        "/opt/homebrew/Cellar/refinery/0.1.0/bin/refinery",
        "/usr/local/Cellar/refinery/0.1.0/bin/refinery",
        "/nix/store/abc-refinery/bin/refinery",
        "/Applications/Refinery.app/Contents/MacOS/refinery",
    ] {
        assert!(
            refuse_managed_location(Path::new(path)).is_err(),
            "{path} must be refused"
        );
    }
    assert!(refuse_managed_location(Path::new("/Users/someone/.local/bin/refinery")).is_ok());
}
