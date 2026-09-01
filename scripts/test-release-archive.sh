#!/usr/bin/env bash
# Verify the release archive produced by release-cli.sh.
#
# This is the same set of checks `refinery update` makes after it downloads an
# archive, run before the archive is published rather than after somebody has
# installed it.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

archive="${1:?usage: scripts/test-release-archive.sh ARCHIVE TARGET}"
target="${2:?usage: scripts/test-release-archive.sh ARCHIVE TARGET}"
version="$(sed -nE 's/^version = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' Cargo.toml)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/refinery-release-test.XXXXXX")"
trap 'rm -rf -- "$work_dir"' EXIT

unzip -Z1 "$archive" | LC_ALL=C sort > "$work_dir/members"
printf '%s\n' refinery refinery-payload.json > "$work_dir/expected"
cmp "$work_dir/expected" "$work_dir/members"

ditto -x -k "$archive" "$work_dir/payload"
/usr/bin/python3 -c 'import json,sys; p=json.load(open(sys.argv[1])); assert p == {"formatVersion":1,"version":sys.argv[2],"target":sys.argv[3],"binaries":["refinery"]}, p' \
  "$work_dir/payload/refinery-payload.json" "$version" "$target"
"$work_dir/payload/refinery" --version | grep -Fx "refinery $version"
printf 'verified release payload %s for %s\n' "$version" "$target"
