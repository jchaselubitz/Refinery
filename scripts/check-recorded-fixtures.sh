#!/usr/bin/env bash
set -euo pipefail

cargo run --quiet --bin refinery-fixtures -- check tests/fixtures/gemini
