# Refinery Stage 1 Implementation Plan

This plan turns `planning/coo-885-refinery-product-description-and-plan.md` into an ordered build sequence for the Stage 1 (Gemini) product. The product description is the authority on scope and behavior; this document is the authority on build order, module responsibilities, and the design decisions the product description intentionally left open.

## Review verdict

The product description is internally consistent and implementable as written. Five points were underspecified and are resolved here rather than by amending the product document:

1. **Backend observation channel.** The `AgentBackend` trait defines `start`, `answer`, and `cancel`, but no path for the orchestrator to observe progress, tool calls, questions, or final output. Resolution: `start` receives a `BackendContext` handle owning the repository tools, the question service, and an event sink; the backend drives the case through that handle rather than returning a stream. This matches Stage 1, where the Gemini tool loop is Refinery-owned, and still fits Stage 2, where the Codex adapter translates App Server events into the same sink calls.
2. **Resume semantics for Gemini.** Gemini's API is stateless per request; "resumable conversation" means Refinery persists the canonical conversation history in `backend_runs` and resumes by replaying that history with the function response appended. No provider-side session is assumed.
3. **Delivery failure position in the state machine.** A failed delivery attempt keeps the case in `delivering` with the failure recorded on the `deliveries` row and a scheduled retry. When retries are exhausted or the error is non-retryable, the case moves to `failed` with the output preserved; a user-initiated retry creates a new delivery attempt from `failed` back through `delivering`. `ready` is only ever a transit state before the first attempt.
4. **Provider media expiry.** Gemini file uploads expire server-side. The attachment record stores the provider file name, upload time, and expiry; any run step that references an expired upload re-uploads from the local media store before continuing. Expiry is never a case failure.
5. **Credential-store fallback.** Where no OS credential store is available (headless Linux without a secret service), setup falls back to a `0600` file inside the data directory with an explicit, visible warning, and `refinery doctor` reports which storage is in use.

One housekeeping note outside this repo's files: the mission's standing architecture artifact still describes the earlier Node/TypeScript recommendation; the Rust baseline in the product description supersedes it.

## Ground rules for the build

- One binary crate, modules as drawn in the product description's repository layout. No workspace, no extracted crates in Stage 1.
- Every externally visible contract (`RefinementRequest`, `QuestionRequest`, `Answer`, `RefinedPrompt`, case events, API error shape) is a Serde type with a Schemars-generated JSON Schema committed under `schemas/`. A CI check fails when generated schemas drift from committed ones.
- All state changes go through the storage layer inside a transaction that writes both the snapshot row and the corresponding `case_events` row. No module mutates a case without appending its event.
- Jobs are the only way long-running work runs. API handlers enqueue and return; they never do provider I/O inline.
- Per-case serialization: a case's jobs share a case-scoped lease so only one operation mutates a given case at a time; independent cases run concurrently on the Tokio runtime.
- Errors carry a retry classification (`retryable`, `non_retryable`, `needs_user`) at the boundary where they occur; the job runner and delivery machinery act on classification, never on error text.

## Milestones

Milestones are strictly ordered by dependency; within a milestone, items can proceed in parallel. Each has exit criteria that must pass before the next begins, because later milestones build on the invariants earlier ones establish.

### M0 — Scaffold and skeleton

Scope:

- Cargo project, Rust stable, edition pinned; Tokio, Axum, Clap, Serde, Schemars, SQLx (SQLite), Reqwest, Tracing dependencies.
- Module tree from the product description (`cli/`, `config/`, `domain/`, `cases/`, `agent/`, `interactions/`, `repositories/`, `media/`, `storage/`, `jobs/`, `api/`, `integrations/`, `diagnostics/`) with empty-but-compiling modules.
- Config loading: data directory resolution (platform-appropriate default, `REFINERY_DATA_DIR` override), non-secret settings file, structured logging to a rotating local file plus stderr.
- Error type strategy: one application error enum with retry classification, `thiserror` at module boundaries.
- CI: fmt, clippy (deny warnings), test, schema-drift check placeholder.

Exit criteria: `refinery --help` runs; `cargo test` and clippy pass in CI; data directory is created with correct permissions on first run.

### M1 — Contracts and state machine (freeze point)

Scope:

- Domain types in `domain/`: `RefinementRequest`, transcript message, attachment metadata, `QuestionRequest`/`Question`/`Answer`, `RefinedPrompt`, `CaseState`, `CaseEvent`, `BackendCapabilities`, delivery envelope for Overlord.
- All wire types versioned with an explicit `schema_version` field; Schemars generation wired into `schemas/` with the drift check active.
- The case state machine as a pure function: `transition(state, event) -> Result<state>`. Every legal edge from the product description encoded, including repeated `awaiting_answer -> running`, the delivery-failure rules from the review verdict, and `cancelled` reachable from every non-terminal state.
- Deterministic validation for `RefinedPrompt`: schema validity, non-empty acceptance criteria, bounded sizes, no unresolved required questions, no provider markup markers; repository-path validity is checked later where a repository handle exists.

Exit criteria: state-machine table tests cover every legal and a sample of illegal transitions; schemas committed; contract serialization round-trip tests pass. After this milestone, contract changes require a schema-version bump.

### M2 — Storage, events, jobs, and recovery

Scope:

- SQLx migrations for the full initial schema from the product description: `cases`, `case_inputs`, `attachments`, `case_events`, `backend_runs`, `question_requests`, `answers`, `outputs`, `deliveries`, `repositories`, `jobs`, `settings`.
- Storage layer in `storage/` exposing intent-level operations (`create_case_idempotent`, `record_question`, `apply_answer`, `record_output`, …), each transactionally pairing snapshot mutation with event append.
- Idempotent ingress: same `idempotency_key` plus equal content digest returns the existing case; same key with different digest is a conflict error.
- Job runner in `jobs/`: claim-with-lease loop, heartbeat-based lease renewal, expiry-based reclaim, bounded attempts, exponential backoff with jitter, stable operation identifiers stored on the job row.
- Startup recovery: reclaim expired leases, resume cases in non-terminal states by enqueueing the job their state implies.

Exit criteria: crash-recovery test (kill mid-job, restart, case completes); expired-lease reclaim test; idempotent-ingress and conflict tests; concurrent-case test proving per-case serialization.

### M3 — CLI, setup, credentials, doctor

Scope:

- `refinery setup` guided flow per the product description's seven steps, using the credential store with the fallback rule from the review verdict.
- `refinery status`, `refinery doctor` (all checks listed in the operational model, each independently reporting pass/warn/fail with a remedy line), `refinery service start|stop` (launchd on macOS, systemd user unit on Linux), `refinery provider configure gemini`, `refinery repository add|list`.
- Provider connection test that authenticates and fetches model metadata without printing the secret.
- Secret redaction layer applied to logs and error display before any provider client exists to leak anything.

Exit criteria: setup completes on a clean machine without editing any file; doctor detects and explains each induced failure (missing key, dead service, bad permissions); redaction tests pass.

### M4 — Repository connector

Scope:

- Registration by canonical path (symlinks resolved at registration), stored policy: max file bytes, max results, ignore-rule handling via the `ignore` crate, optional exclusion globs for secrets.
- The five tools — `list_files`, `read_file`, `search_text`, `git_status`, `git_diff` — as plain functions with typed inputs/outputs, byte and item limits enforced inside the connector, binary detection rejecting non-text reads.
- Path safety: every requested path canonicalized and prefix-checked against the root after symlink resolution; traversal attempts return a policy error and an audit event.
- Per-case audit events for every tool invocation (tool, arguments digest, result size).

Exit criteria: adversarial test suite passes — `..` traversal, absolute paths, symlinks escaping the root, symlink swap between check and read (open-then-verify), oversized files, binary files, ignored-file policy.

### M5 — Media store and Gemini upload lifecycle

Scope:

- Case-scoped media directory; import verifies declared media type by sniffing, enforces size caps, records digest.
- Gemini Files API client: upload, activation polling for video, provider file name and expiry persisted on the attachment, re-upload on expiry per the review verdict.
- Attachment lifecycle states: `imported`, `uploading`, `available`, `expired`, `rejected`.

Exit criteria: import validation tests (type mismatch, oversize, digest mismatch); upload lifecycle tests against recorded fixtures; expiry-triggered re-upload test.

### M6 — Gemini agent backend

Scope:

- `AgentBackend` trait with the `BackendContext` design from the review verdict; Gemini as the sole implementation in `agent/gemini.rs`.
- The tool loop: system instruction (refinement role, tool doctrine, untrusted-content rule), declared functions for the five repository tools plus `ask_user` plus `submit_refined_prompt`, iterative `generateContent` calls with full history persisted to `backend_runs` after every step.
- `ask_user` handling per the product description's seven-step sequence: persist request, transition to `awaiting_answer`, complete the current job, and let answer arrival enqueue a resume job that replays history with the function response appended. One outstanding question request per case, enforced in storage.
- Structured output via `submit_refined_prompt` carrying the `RefinedPrompt` schema; malformed submissions are returned to the model once with the validation errors, then fail the case.
- Provider error mapping to retry classifications; token/usage metadata recorded on `backend_runs`.
- Prompt-injection posture: repository and transcript content wrapped as data, tool set fixed regardless of content instructions; adversarial fixtures exercise this.

Exit criteria: recorded-fixture tests for a full refinement, a question round-trip, and a malformed-output retry; restart-while-awaiting-answer test resumes correctly; live opt-in smoke test completes a real refinement.

### M7 — Local API, Overlord ingress, and delivery

Scope:

- Axum loopback server, locally generated bearer token stored via the credential machinery, uniform error body.
- Routes from the product description plus two additions the UI needs: `GET /v1/refinements` (bounded list with state filter) and `GET /v1/settings` (non-secret). `GET /v1/refinements/{id}/events` serves both JSON backlog and SSE via content negotiation.
- Overlord ingress: `POST /v1/refinements` accepts the versioned `RefinementRequest`, applies idempotent ingress, returns the case identity immediately.
- Question forwarding: outbound notification to the submitting source's declared callback (local Overlord endpoint) with retry; the local UI path needs no callback since it polls/SSEs.
- Delivery: destination adapter interface with two implementations — local Overlord submission (idempotency key per attempt set, response metadata recorded) and local export (write the prompt to a file / stdout via CLI). Retry per the classification rules; `deliveries` rows record every attempt.

Exit criteria: end-to-end test with a fake Overlord server — submit, question out, answer in, delivery received exactly once under injected retries and a mid-flight restart; auth tests (missing/wrong token); SSE update test.

### M8 — Local browser interface

Scope:

- Minimal static frontend embedded via `include_dir` (or equivalent), served by the daemon; no separate deploy, no framework heavier than needed for forms and an event stream.
- Views: case list, case detail (state, inputs, questions, output, deliveries, bounded events), pending-question answer form supporting the three response types, repository list/registration, settings and health, log tail.
- `refinery open` launches the browser with a tokened URL.

Exit criteria: every completion-criteria interaction (answer a question, retry a delivery, cancel a case, copy the prompt) works through the UI against a live local daemon; accessibility pass on the forms.

### M9 — Quality, evaluations, and Stage 1 acceptance

Scope:

- Fill remaining deterministic-test gaps against the product description's quality-strategy list.
- Sanitized recorded-fixture harness formalized (record mode gated behind an env flag, secrets scrubbed at record time).
- Versioned evaluation set: transcripts plus repository fixtures scoring self-containment, faithfulness, grounding, explicitness, and question economy; a scripted run reports per-case scores.
- Instrument the product success measures (case funnel counts, question rate, resume reliability, delivery retries, validation failures) as local metrics visible in `refinery status`.
- Run the Stage 1 completion-criteria checklist from the product description verbatim; each item gets an automated or scripted manual check.

Exit criteria: the product description's nine Stage 1 completion criteria all pass, including the repository-boundary and prompt-injection suites.

## Cross-cutting decisions

- **Concurrency model.** The daemon is a single Tokio process. The job runner bounds concurrent case jobs (configurable, default small). SQLite runs in WAL mode with a single writer connection pool sized accordingly; storage operations keep transactions short.
- **Time and identifiers.** UUIDv7 for cases, events, jobs, and deliveries; all timestamps UTC in storage and wire formats.
- **Bounded everything.** Transcript size, attachment count/size, question count per request, tool-result bytes, event-history reads, and refined-prompt size all have configured caps with defaults set in M1 contracts.
- **No premature abstraction.** Exactly one backend, two destination adapters, one source. Traits exist where Stage 2 provably needs them (`AgentBackend`, destination adapter) and nowhere else.

## Sequencing and estimate shape

M0–M2 are the foundation and must be sequential. M3 and M4 can proceed in parallel after M2. M5 needs M2; M6 needs M4 and M5. M7 needs M2 and benefits from M6 but its fake-backend tests can start once contracts exist. M8 needs M7. M9 runs throughout and closes last.

The critical path is M0 → M1 → M2 → M6 → M7 → M9: contracts, durability, the agent loop, and the Overlord round-trip. The browser interface and CLI polish are off the critical path and can absorb schedule pressure without threatening the completion criteria.

Stage 2 (Codex App Server) begins only after M9's evaluation baseline is accepted, per the product description.
