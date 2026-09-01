//! The published-release feed, reduced to the four facts an update needs:
//! which version is newest, where its archive is, where its checksums are, and
//! what the archive should hash to.
//!
//! Parsing lives here, apart from the network and the filesystem, because that
//! is the part worth asserting: a release whose asset naming drifts must fail
//! with a diagnosis rather than install the wrong file.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// The repository whose releases `refinery update` installs from.
///
/// A compiled-in constant rather than a setting or an environment variable: an
/// updater that can be pointed at another origin by configuration is a way to
/// hand somebody a different binary. Tests substitute the whole
/// [`super::ReleaseSource`] instead.
pub const DEFAULT_RELEASE_REPO: &str = "cooperativ-labs/refinery";

/// A three-component version, ordered by component.
///
/// Refinery versions are `0.YYMMDDHHMM.0` (`scripts/bump-minor-version.sh`), so
/// numeric ordering on the middle component is chronological ordering. Parsing
/// as numbers rather than comparing strings is what makes `0.2608101202.0`
/// newer than `0.2608100841.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// Major component.
    pub major: u64,
    /// Minor component; the release timestamp in practice.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
}

impl Version {
    /// Parse `1.2.3`, with or without the `v` a Git tag carries.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let digits = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let mut parts = digits.split('.');
        let mut next = |component: &str| -> Result<u64> {
            parts
                .next()
                .ok_or_else(|| {
                    AppError::invalid(format!("version {raw} has no {component} component"))
                })?
                .parse::<u64>()
                .map_err(|_| {
                    AppError::invalid(format!(
                        "version {raw} has a non-numeric {component} component"
                    ))
                })
        };
        let major = next("major")?;
        let minor = next("minor")?;
        let patch = next("patch")?;
        if parts.next().is_some() {
            return Err(AppError::invalid(format!(
                "version {raw} has more than three components"
            )));
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One downloadable file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// File name as published.
    pub name: String,
    /// Direct download URL.
    #[serde(rename = "browser_download_url")]
    pub url: String,
}

/// A published release, as much of it as an update needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Tag the release was cut from, for example `v0.2608101202.0`.
    pub tag: String,
    /// Version parsed out of the tag.
    pub version: Version,
    /// Human-facing release page.
    pub url: String,
    /// Files attached to the release.
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseDocument {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

impl Release {
    /// Parse one release object from the GitHub releases API.
    pub fn parse(document: &str) -> Result<Self> {
        let parsed: ReleaseDocument =
            serde_json::from_str(document).map_err(|source| AppError::Upstream {
                service: "github",
                message: format!(
                    "the release feed was not the JSON document this build understands: {source}"
                ),
                retry: crate::error::RetryClass::Retryable,
            })?;
        let version = Version::parse(&parsed.tag_name).map_err(|source| {
            AppError::invalid(format!(
                "release tag {} is not a Refinery version: {}",
                parsed.tag_name,
                source.detail()
            ))
        })?;
        Ok(Self {
            tag: parsed.tag_name,
            version,
            url: parsed.html_url,
            assets: parsed.assets,
        })
    }

    /// The CLI archive for `target`, preferring the ZIP that current releases
    /// publish and accepting a tarball.
    ///
    /// Matching on the target suffix rather than reconstructing the whole file
    /// name means a release whose archive naming gains a component still
    /// resolves, while a release that has no build for this machine still
    /// fails.
    pub fn cli_archive(&self, target: &str) -> Option<&Asset> {
        let candidates = |extension: &str| {
            let suffix = format!("-{target}{extension}");
            self.assets
                .iter()
                .find(|asset| asset.name.starts_with("refinery-") && asset.name.ends_with(&suffix))
        };
        candidates(".zip").or_else(|| candidates(".tar.gz"))
    }

    /// The checksum manifest published alongside the archives.
    pub fn checksums(&self) -> Option<&Asset> {
        self.assets
            .iter()
            .find(|asset| asset.name == "checksums.txt")
    }
}

/// Look up the expected digest of `asset_name` in a `shasum`-format manifest.
///
/// Entries are written from the release working directory, so the recorded path
/// is `dist/refinery-…`; only the file name is meaningful to a client.
pub fn expected_digest(manifest: &str, asset_name: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let path = fields.next()?;
        let recorded = path.trim_start_matches('*').rsplit('/').next()?;
        (recorded == asset_name && digest.len() == 64).then(|| digest.to_ascii_lowercase())
    })
}

/// The release target triple for the machine this binary is running on.
pub fn host_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        (os, arch) => Err(AppError::unsupported(
            format!(
                "`refinery update` on {arch}-{os}: only macOS builds are published. \
                 Build from source with `cargo build --release`"
            ),
            "when another platform is published",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"{
        "tag_name": "v0.2608101202.0",
        "html_url": "https://github.com/cooperativ-labs/refinery/releases/tag/v0.2608101202.0",
        "draft": false,
        "assets": [
            {"name": "checksums.txt",
             "browser_download_url": "https://example.invalid/checksums.txt"},
            {"name": "refinery-0.2608101202.0-aarch64-apple-darwin.zip",
             "browser_download_url": "https://example.invalid/arm.zip"},
            {"name": "refinery-0.2608101202.0-x86_64-apple-darwin.zip",
             "browser_download_url": "https://example.invalid/intel.zip"}
        ]
    }"#;

    #[test]
    fn versions_order_by_number_and_not_by_string() {
        let older = Version::parse("0.2608100841.0").expect("older");
        let newer = Version::parse("0.2608101202.0").expect("newer");
        assert!(newer > older);
        // String ordering would put "0.99.0" after "0.100.0"; numbers do not.
        assert!(Version::parse("0.100.0").unwrap() > Version::parse("0.99.0").unwrap());
        assert_eq!(
            Version::parse("v1.2.3").unwrap(),
            Version::parse("1.2.3").unwrap()
        );
    }

    #[test]
    fn a_version_that_is_not_three_numbers_is_rejected() {
        for raw in ["", "1.2", "1.2.3.4", "1.2.x", "latest"] {
            assert!(Version::parse(raw).is_err(), "{raw} must not parse");
        }
    }

    #[test]
    fn the_feed_resolves_the_archive_for_this_machine_only() {
        let release = Release::parse(FEED).expect("parse feed");
        assert_eq!(release.version, Version::parse("0.2608101202.0").unwrap());
        assert_eq!(
            release
                .cli_archive("aarch64-apple-darwin")
                .map(|asset| asset.url.as_str()),
            Some("https://example.invalid/arm.zip")
        );
        assert_eq!(
            release
                .cli_archive("x86_64-apple-darwin")
                .map(|asset| asset.url.as_str()),
            Some("https://example.invalid/intel.zip")
        );
        assert!(release.cli_archive("aarch64-unknown-linux-gnu").is_none());
        assert!(release.checksums().is_some());
    }

    #[test]
    fn a_release_that_ships_a_tarball_resolves() {
        let feed = r#"{
            "tag_name": "v0.2608101059.0",
            "html_url": "https://example.invalid/tag",
            "assets": [
                {"name": "refinery-0.2608101059.0-aarch64-apple-darwin.tar.gz",
                 "browser_download_url": "https://example.invalid/arm.tar.gz"}
            ]
        }"#;
        let release = Release::parse(feed).expect("parse feed");
        assert_eq!(
            release
                .cli_archive("aarch64-apple-darwin")
                .map(|asset| asset.name.as_str()),
            Some("refinery-0.2608101059.0-aarch64-apple-darwin.tar.gz")
        );
    }

    #[test]
    fn a_feed_without_a_version_tag_is_an_error_and_not_an_update() {
        let feed = r#"{"tag_name":"nightly","html_url":"https://example.invalid","assets":[]}"#;
        assert!(Release::parse(feed).is_err());
    }

    #[test]
    fn digests_are_matched_by_file_name_and_not_by_recorded_path() {
        let manifest = "\
fc9950aeea35633a058d5cf4cea3ce1e0dadd9f64f75fd768eed3dd3bf283a94  dist/refinery-1.0.0-aarch64-apple-darwin.zip
beb1f7660ead14276a14733391ad369f4dc2f8b6642890d5a96b19539832256b  dist/refinery-1.0.0-x86_64-apple-darwin.zip
";
        assert_eq!(
            expected_digest(manifest, "refinery-1.0.0-x86_64-apple-darwin.zip").as_deref(),
            Some("beb1f7660ead14276a14733391ad369f4dc2f8b6642890d5a96b19539832256b")
        );
        assert_eq!(expected_digest(manifest, "checksums.txt"), None);
    }

    #[test]
    fn a_truncated_digest_line_does_not_count_as_a_match() {
        let manifest = "abc123  dist/refinery-1.0.0-aarch64-apple-darwin.zip\n";
        assert_eq!(
            expected_digest(manifest, "refinery-1.0.0-aarch64-apple-darwin.zip"),
            None
        );
    }
}
