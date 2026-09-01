# Stamp the crate version as 0.YYMMDDHHMM.0 from the current UTC time.
bump-minor:
    ./scripts/bump-minor-version.sh

build-debug:
    cargo build

build-release:
    cargo build --release

test:
    cargo test

check:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    ./scripts/check-schema-drift.sh
    ./scripts/check-interface-assets.sh
    ./scripts/check-recorded-fixtures.sh
    ./scripts/run-evaluations.sh

schema-check:
    ./scripts/check-schema-drift.sh

# Parse the embedded interface script. Needs node; skipped without it.
interface-check:
    ./scripts/check-interface-assets.sh

fixture-check:
    ./scripts/check-recorded-fixtures.sh

eval:
    ./scripts/run-evaluations.sh

acceptance:
    ./scripts/check-stage1-acceptance.sh

# Regenerate the committed JSON Schemas from the contract types.
schemas:
    cargo run --bin refinery-schemas -- --out schemas

# Build the release archive for this machine into dist/. Set
# REFINERY_CODESIGN_IDENTITY and REFINERY_NOTARY_PROFILE to sign and notarize.
release-archive target="":
    ./scripts/release-cli.sh {{target}}

# Verify a built archive the way `refinery update` verifies a downloaded one.
release-verify archive target:
    ./scripts/test-release-archive.sh {{archive}} {{target}}
