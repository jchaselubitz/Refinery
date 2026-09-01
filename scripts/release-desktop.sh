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

# Remove only release artifacts and the two cross-compiled target directories.
# Keeping the host target directory avoids discarding unrelated local builds.
rm -f dist/refinery-*.zip dist/refinery-*.zip.sha256 dist/checksums.txt
for target in "${targets[@]}"; do
  rm -rf "target/$target"
done

for target in "${targets[@]}"; do
  REFINERY_RELEASE_TAG="$tag" scripts/release-cli.sh "$target"
  scripts/test-release-archive.sh "dist/refinery-$version-$target.zip" "$target"
done

cat dist/refinery-*.zip.sha256 > dist/checksums.txt
(cd dist && shasum -a 256 -c checksums.txt)

# GitHub's target_commitish accepts a branch or full SHA, not the local ref HEAD.
commit="$(git rev-parse HEAD)"
gh release create "$tag" dist/refinery-*.zip dist/checksums.txt \
  --target "$commit" \
  --title "Refinery $tag" \
  --generate-notes
