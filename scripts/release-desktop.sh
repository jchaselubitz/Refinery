#!/usr/bin/env bash
# Build, verify, and publish the signed/notarized macOS Refinery archives.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

targets=(aarch64-apple-darwin x86_64-apple-darwin)
version="$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' Cargo.toml)"
if [[ -z "$version" ]]; then
  echo "Could not read the crate version from Cargo.toml" >&2
  exit 1
fi
tag="v$version"

command -v gh >/dev/null || { echo "GitHub CLI (gh) is required" >&2; exit 1; }
gh auth status >/dev/null
: "${REFINERY_CODESIGN_IDENTITY:?Set the Developer ID identity used to sign the release}"
: "${REFINERY_NOTARY_PROFILE:?Set the notarytool keychain profile used to notarize the release}"

# Remove only release artifacts and the two cross-compiled target directories.
# Keeping the host target directory avoids discarding unrelated local builds.
rm -f dist/refinery-*.zip dist/refinery-*.zip.sha256 dist/checksums.txt
for target in "${targets[@]}"; do
  rm -rf "target/$target"
done

for target in "${targets[@]}"; do
  REFINERY_RELEASE_TAG="$tag" \
    REFINERY_CODESIGN_IDENTITY="$REFINERY_CODESIGN_IDENTITY" \
    REFINERY_NOTARY_PROFILE="$REFINERY_NOTARY_PROFILE" \
    scripts/release-cli.sh "$target"
  scripts/test-release-archive.sh "dist/refinery-$version-$target.zip" "$target"
done

cat dist/refinery-*.zip.sha256 > dist/checksums.txt
(cd dist && shasum -a 256 -c checksums.txt)

# GitHub's target_commitish accepts a branch or full SHA, not the local ref HEAD.
commit="$(git rev-parse HEAD)"
assets=(dist/refinery-*.zip dist/checksums.txt)
if gh release view "$tag" >/dev/null 2>&1; then
  gh release upload "$tag" "${assets[@]}" --clobber
else
  if ! gh release create "$tag" "${assets[@]}" \
    --target "$commit" \
    --title "Refinery $tag" \
    --generate-notes; then
    # A tag push can start the release workflow while this local build runs.
    # If it won that race, publish this verified build's assets to the same
    # release instead of failing solely because the release already exists.
    gh release view "$tag" >/dev/null
    gh release upload "$tag" "${assets[@]}" --clobber
  fi
fi
