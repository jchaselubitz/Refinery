#!/usr/bin/env bash
# Build the distributable Refinery payload: the `refinery` binary, with the
# browser interface compiled into it.
#
# Usage: scripts/release-cli.sh [target]
#
# Set REFINERY_CODESIGN_IDENTITY to sign the binary before it is archived. Set
# REFINERY_NOTARY_PROFILE to submit the signed ZIP with notarytool. CI sets both
# after importing the Developer ID certificate into a temporary keychain.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

target="${1:-}"
if [[ -z "$target" ]]; then
  case "$(uname -m)" in
    arm64) target="aarch64-apple-darwin" ;;
    x86_64) target="x86_64-apple-darwin" ;;
    *) echo "Unsupported host architecture: $(uname -m)" >&2; exit 1 ;;
  esac
fi

case "$target" in
  aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *) echo "Unsupported release target: $target" >&2; exit 1 ;;
esac

version="$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' Cargo.toml)"
if [[ -z "$version" ]]; then
  echo "Could not read the crate version from Cargo.toml" >&2
  exit 1
fi

# A tag that disagrees with the manifest would publish an archive whose payload
# manifest the updater then refuses, so the mismatch is caught here instead.
if [[ -n "${REFINERY_RELEASE_TAG:-}" && "$REFINERY_RELEASE_TAG" != "v$version" ]]; then
  echo "Release tag $REFINERY_RELEASE_TAG does not match crate version v$version" >&2
  exit 1
fi

output_dir="${REFINERY_RELEASE_DIR:-dist}"
archive_name="refinery-${version}-${target}.zip"
stage_dir="$output_dir/.stage-$target"

cargo build --locked --release --bin refinery --target "$target"

rm -rf "$stage_dir"
mkdir -p "$stage_dir"
cp "target/$target/release/refinery" "$stage_dir/refinery"

# Refuse a mis-stamped archive before it reaches signing and notarization. The
# updater repeats these checks after extraction because an archive is the unit
# of distribution, but catching it here makes the failure immediate and
# target-specific.
"$stage_dir/refinery" --version | grep -Fx "refinery $version" >/dev/null
printf '{"formatVersion":1,"version":"%s","target":"%s","binaries":["refinery"]}\n' \
  "$version" "$target" > "$stage_dir/refinery-payload.json"

if [[ -n "${REFINERY_CODESIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp \
    --sign "$REFINERY_CODESIGN_IDENTITY" "$stage_dir/refinery"
  codesign --verify --strict --verbose=2 "$stage_dir/refinery"
fi

mkdir -p "$output_dir"
archive_path="$(cd "$output_dir" && pwd -P)/$archive_name"
rm -f "$archive_path"
(cd "$stage_dir" && /usr/bin/zip -q -X "$archive_path" refinery refinery-payload.json)

if [[ -n "${REFINERY_NOTARY_PROFILE:-}" ]]; then
  : "${REFINERY_CODESIGN_IDENTITY:?REFINERY_NOTARY_PROFILE requires REFINERY_CODESIGN_IDENTITY}"
  xcrun notarytool submit "$archive_path" --keychain-profile "$REFINERY_NOTARY_PROFILE" --wait
fi

rm -rf "$stage_dir"

checksum="$(shasum -a 256 "$archive_path" | awk '{print $1}')"
printf '%s  %s\n' "$checksum" "$archive_name" > "$archive_path.sha256"
printf '%s\n' "$archive_path"
