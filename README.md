# Refinery

Refinery turns transcripts, repository context, images, video, and follow-up
answers into validated prompts for destinations such as Overlord. It is a single
local binary: one daemon, one SQLite database, one data directory, no cloud
component.

- Product authority: [`planning/coo-885-refinery-product-description-and-plan.md`](planning/coo-885-refinery-product-description-and-plan.md)
- Build order: [`planning/coo-885-refinery-implementation-plan.md`](planning/coo-885-refinery-implementation-plan.md)

## Status

Stage 1 is complete. `refinery serve` binds the loopback API and the durable job
loop, so a case submitted through Overlord ingress is prepared, refined, and
delivered without anything else being started; `refinery open` launches a
browser at a tokened URL onto an interface compiled into the binary. From that
page a person can watch the case list and a case's live event stream, answer a
pending question in free-text, single-choice, and multiple-choice form, copy
the refined prompt, retry a failed delivery, cancel a running case, register a
repository, and read health, settings, and the log tail.

Milestones M0–M8 delivered the command surface, configuration, logging, and
error model; M1 froze the contracts, schemas, state machine, and validation;
M2 delivered storage, events, jobs, and recovery; M3 setup, credentials, and
doctor; M4 the read-only repository connector; M5 the media store and provider
uploads; M6 the Gemini agent loop; M7 the local API, ingress, and delivery.
M8 added the embedded browser interface and live daemon composition. M9 adds
durable product metrics, gated and sanitized provider-fixture recording, the
versioned refinement-quality evaluation set, and an executable acceptance
check. All nine Stage 1 completion criteria pass.

Changing a contract in `src/domain/` from here on means bumping
`CONTRACT_VERSION`, regenerating `schemas/`, and updating the frozen fixtures in
`tests/fixtures/contracts/`. See [`schemas/README.md`](schemas/README.md).

## Install

macOS, Apple Silicon or Intel:

```sh
curl -fsSL https://raw.githubusercontent.com/cooperativ-labs/refinery/main/scripts/install-cli.sh | bash
refinery setup
```

The script installs the newest notarized release into `~/.local/bin`. Nothing is
written until the download matches the checksum the release published, its
payload manifest names that version and target, and the binary passes
Gatekeeper's signature check. Other platforms build from source with
`cargo build --release`.

```sh
refinery update            # install the newest release over this one
refinery update --check    # report what is available without installing it
refinery update --force    # reinstall the published version, to repair a copy
refinery uninstall         # stop the service, drop the secrets, remove the binary
refinery uninstall --purge # also delete the data directory
```

`update` refuses a copy owned by Homebrew, Nix, or an application bundle rather
than diverging it from its package, and `doctor` reports which case this install
is before you need it. `uninstall` keeps the data directory unless `--purge`
asks for it, because that directory holds your cases rather than anything the
installer put there; it asks for confirmation on a terminal and requires `--yes`
anywhere else.

## Publishing a release

Releases are cut from a tag and built, signed, notarized, and attested by
[`.github/workflows/release-cli.yml`](.github/workflows/release-cli.yml):

```sh
just bump-minor                       # stamp 0.YYMMDDHHMM.0 into Cargo.toml
git commit -am "Release v$(sed -nE 's/^version = "(.*)"$/\1/p' Cargo.toml)"
git tag "v$(sed -nE 's/^version = "(.*)"$/\1/p' Cargo.toml)"
git push origin main --tags
```

To build and publish the two desktop archives from this Mac instead, run
`just release-desktop`. It removes the previous `dist/refinery-*` archives and
the two cross-compiled macOS target directories before rebuilding, verifies the
new archives, and creates the GitHub release with the GitHub CLI. It uses the
same `REFINERY_CODESIGN_IDENTITY` and `REFINERY_NOTARY_PROFILE` environment
variables as `just release-archive` for signing and notarization; both are
required, so an unsigned or unnotarized desktop release cannot be published.
If the tag-triggered workflow creates the release first, the command replaces
that release's matching archives instead of failing on the existing tag.

The workflow builds `aarch64-apple-darwin` and `x86_64-apple-darwin`, signs each
binary with the Developer ID identity, notarizes the archive, verifies it with
[`scripts/test-release-archive.sh`](scripts/test-release-archive.sh), and
publishes both archives plus `checksums.txt` to a GitHub release. It needs these
repository secrets: `APPLE_SIGNING_IDENTITY`, `APPLE_CERTIFICATE_BASE64`,
`APPLE_CERTIFICATE_PASSWORD`, `KEYCHAIN_PASSWORD`, `APPLE_ID`,
`APPLE_APP_SPECIFIC_PASSWORD`, and `APPLE_TEAM_ID`.

An archive holds the `refinery` binary and a `refinery-payload.json` manifest
naming the version, target, and binary it contains. `refinery update` and the
install script both check that manifest, so an archive whose contents drift from
what a release claims fails to install rather than half-installing. Build one
locally with `just release-archive`; without the signing variables set it is
unsigned but otherwise identical.

## Build and run

```sh
cargo build
cargo run -- --help
cargo run -- setup      # guided first run; needs a terminal
cargo run -- status     # includes funnel, questions, resume, retry, and validation metrics
cargo run -- doctor     # add --offline to skip the provider check
cargo run -- serve      # the daemon: loopback API, interface, and job loop
cargo run -- open       # launch a browser at the interface; --print for the URL
just schemas    # regenerate the committed JSON Schemas after a contract change
just eval       # score the versioned refinement-quality corpus
just acceptance # run every Stage 1 completion criterion
```

`just check` runs formatting, clippy with warnings denied, the tests, the
schema-drift check, interface asset parse, fixture sanitizer check, and the
versioned evaluation — the same gates CI runs. The full acceptance mapping is
documented in [`acceptance/stage1.md`](acceptance/stage1.md).

## Desktop window

`refinery-desktop` is a native window over the same daemon, for the four things
a person does most: paste a transcript and submit it, answer the question that
comes back, copy or save the refined prompt, and keep the Gemini key and the
enrolled folders current. It is a second binary behind the `desktop` feature,
because its GUI toolchain is heavy and nothing the CLI does needs it:

```sh
just desktop            # run it from source
just build-desktop      # target/release/refinery-desktop
just desktop-check      # clippy and the window's own tests, feature on
```

The window resolves the data directory, port, and credential store exactly as
the command line does, so it shares cases, folders, and the key with `refinery
serve`, `refinery open`, and Overlord. If a daemon is already listening — the
installed service, or a `refinery serve` in a terminal — the window attaches to
it; otherwise it hosts the daemon itself for as long as it is open. Its status
line says which.

| Tab          | What it does                                                                                        |
| ------------ | --------------------------------------------------------------------------------------------------- |
| Compose      | Paste text; `User:` / `Assistant:` / `System:` line prefixes become separate messages. Pick a folder and where the prompt goes, then submit. |
| Cases        | Every case, live. Answer a pending question, copy the prompt or save it as Markdown, retry a delivery, cancel. |
| Repositories | Enroll a folder with a native picker, or forget one. Nothing on disk is modified.                    |
| Settings     | Store, test, or remove the Gemini key; see the provider, model, data directory, and service address. |

A case submitted from the window keeps its result locally by default, exported
to `exports/` inside the data directory, and the Cases tab shows the prompt as
soon as it exists; an Overlord destination is offered when `refinery setup` has
configured one. The window identifies itself as source `local_ui` with instance
`refinery-desktop`.

On macOS the key lives in the login keychain under the same item the CLI uses,
so the first read from the window may prompt for permission to share it; allow
it once. `REFINERY_DESKTOP_TAB=cases|repositories|settings` opens the window on
that tab, which is handy for screenshots and support.

## Data directory

Refinery keeps everything it owns in one owner-only directory, created on first
run:

| Path            | Contents                                          |
| --------------- | ------------------------------------------------- |
| `refinery.toml` | Non-secret settings                               |
| `refinery.db`   | SQLite state                                      |
| `media/`        | Case-scoped attachment bytes                      |
| `logs/`         | Rotating structured logs                          |
| `credentials/`  | Credential-store fallback, where no store exists   |
| `run/`          | Runtime state such as the service lock             |

It defaults to the platform application-data location and is overridden with
`REFINERY_DATA_DIR` or `--data-dir`. Set `REFINERY_CREDENTIALS=file` to keep
secrets in the data directory even where the operating system has a credential
store — the escape hatch for a broken or prompting keyring, and what the test
suite uses so it never reads the credential store of the machine running it.
Secrets are never written to the settings
file; they live in the operating-system credential store.

## Layout

```text
src/
  main.rs      executable entry point
  lib.rs       crate root
  app.rs       composition root
  cli/         command surface
  config/      data-directory resolution and non-secret settings
  domain/      wire and internal contracts, frozen in M1
  cases/       state machine (M1), orchestration (M2), and the job loop (M8)
  agent/       agent backend boundary and Gemini backend (M6)
  interactions/ questions and answers (M6)
  repositories/ read-only repository connector (M4)
  media/       local media store and provider uploads (M5)
  storage/     SQLite schema and intent-level operations (M2)
  jobs/        durable job runner (M2)
  api/         loopback HTTP API (M7) and the embedded interface (M8)
  integrations/ destination adapters (M7)
  diagnostics/ logging, status, and doctor
web/static/    the interface's three source files, embedded into the binary
```

See [`web/README.md`](web/README.md) for the rules the interface is held to and
how to work on it.
