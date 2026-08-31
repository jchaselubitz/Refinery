#!/usr/bin/env bash
# Set the crate version to 0.YYMMDDHHMM.0 using the current UTC time.
# Usage: ./scripts/bump-minor-version.sh

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$repo_root/Cargo.toml"
timestamp="$(date -u +%y%m%d%H%M)"
next_version="0.${timestamp}.0"

if [[ ! -f "$manifest" ]]; then
  echo "No Cargo.toml at $manifest; scaffold the crate before bumping the version." >&2
  exit 1
fi

if ! grep -Eq '^version = "[0-9]+\.[0-9]+\.[0-9]+"$' "$manifest"; then
  echo "Could not find the version in $manifest" >&2
  exit 1
fi

if grep -Fq "version = \"$next_version\"" "$manifest"; then
  echo "Refusing to reuse version $next_version; run again after the minute changes." >&2
  exit 1
fi

perl -0pi -e "s/^version = \"[0-9]+\\.[0-9]+\\.[0-9]+\"\$/version = \"$next_version\"/m" "$manifest"

# Cargo preserves existing dependency selections when it resolves an existing
# lockfile. Running metadata after changing the package version updates only
# the workspace package records, so release builds can stay reproducible with
# --locked.
(cd "$repo_root" && cargo metadata --format-version 1 >/dev/null)
(cd "$repo_root" && cargo metadata --locked --format-version 1 >/dev/null)

echo "Updated manifest and lockfile to $next_version"
