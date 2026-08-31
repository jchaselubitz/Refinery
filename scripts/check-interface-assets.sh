#!/usr/bin/env bash
# Parse the embedded interface's script.
#
# The interface is served from a table of files compiled into the binary, with
# no build step and no bundler. That is deliberate — it is the reason the page
# has no deploy of its own — but it also means nothing between the editor and
# the browser would notice a syntax error. The accessibility tests read the
# HTML and hold the script to its no-markup rule, and this adds the one thing
# they cannot do without a JavaScript engine: confirm the script parses.
#
# Node is used only as a parser. It is not a build dependency: when it is
# absent the check reports that and succeeds, so a machine without it can still
# run every other gate.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

script="web/static/app.js"

if [[ ! -f "$script" ]]; then
  echo "interface asset check: $script is missing" >&2
  exit 1
fi

if ! command -v node >/dev/null 2>&1; then
  echo "interface asset check: node is not installed; skipping the parse."
  exit 0
fi

if ! node --check "$script"; then
  echo "interface asset check: $script does not parse." >&2
  exit 1
fi

echo "interface asset check: $script parses."
