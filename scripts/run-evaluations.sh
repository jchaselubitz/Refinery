#!/usr/bin/env bash
set -euo pipefail

cargo run --quiet --bin refinery-eval -- --set evaluations/v1 "$@"
