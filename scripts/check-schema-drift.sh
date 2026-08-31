#!/usr/bin/env bash
# Fail when the JSON Schemas committed under schemas/ differ from what the
# current contract types generate.
#
# The committed schemas are the integration contract for Overlord and future
# clients, so they must never drift from the Rust types silently. Contracts
# were frozen in milestone M1: after it, a change to a type in src/domain/ that
# changes its schema is a contract change, and this check is what makes that
# unavoidable rather than aspirational.
#
# On failure, regenerate and review the diff:
#   cargo run --bin refinery-schemas -- --out schemas

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

schema_dir="schemas"
generator="refinery-schemas"

if ! cargo metadata --no-deps --format-version 1 | grep -Fq "\"name\":\"$generator\""; then
  echo "error: the '$generator' generator does not exist, so drift cannot be detected." >&2
  exit 1
fi

generated_dir="$(mktemp -d)"
trap 'rm -rf "$generated_dir"' EXIT

cargo run --quiet --locked --bin "$generator" -- --out "$generated_dir"

# Compare the generated JSON only. schemas/ also holds prose (README.md) that
# no generator produces, so a whole-directory diff would report it as drift.
status=0
while IFS= read -r generated; do
  name="$(basename "$generated")"
  committed="$schema_dir/$name"
  if [[ ! -f "$committed" ]]; then
    echo "error: $name is generated but not committed under $schema_dir/." >&2
    status=1
    continue
  fi
  if ! diff -u "$committed" "$generated"; then
    echo "error: $name differs from what the contract types generate." >&2
    status=1
  fi
done < <(find "$generated_dir" -name '*.json' -type f | sort)

while IFS= read -r committed; do
  name="$(basename "$committed")"
  if [[ ! -f "$generated_dir/$name" ]]; then
    echo "error: $schema_dir/$name is committed but no contract type generates it." >&2
    status=1
  fi
done < <(find "$schema_dir" -name '*.json' -type f | sort)

if [[ $status -ne 0 ]]; then
  echo >&2
  echo "       Regenerate with: cargo run --bin $generator -- --out $schema_dir" >&2
  exit 1
fi

echo "schema drift check: schemas match the contract types."
