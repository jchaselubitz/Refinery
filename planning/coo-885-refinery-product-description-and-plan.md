# Refinery: Product Description and Delivery Plan

## Product summary

Refinery is a local application that turns incomplete, conversational, and multimodal input into a clear prompt that another agent or software system can execute.

A refinement may begin with a transcript, a rough request, images, video, and a reference to a local repository. Refinery gives an AI agent controlled access to that context, lets the agent ask the user targeted follow-up questions, validates the resulting prompt, and delivers it to a destination such as Overlord.

Refinery is not a general chat client and does not execute the refined prompt. Its purpose is to improve the handoff between an initial idea and the system that will perform the work.

The first release is a headless Rust application using Gemini 3.7 Flash. Later releases add ChatGPT subscription access through Codex App Server, followed by local models and user-configured inference providers. A Latch integration may be added later as an optional external-agent connector.

## Product promise

Refinery should make this workflow reliable:

1. A user discusses a task in Overlord or another source.
2. The source submits the conversation and any attachments to Refinery.
3. Refinery grounds an agent in the selected local repository and supplied media.
4. The agent identifies missing information and asks focused questions.
5. The user answers in the interface where the task originated, or in Refinery's local interface.
6. The agent produces a structured, implementation-ready prompt.
7. Refinery validates and sends that prompt to the selected destination.

The user should never need to interact with an agent's terminal, reconstruct lost context, or manually copy intermediate questions and answers between systems.

## Product principles

### Local ownership

Refinement cases, repository access, configuration, credentials, and working state live on the user's machine. Refinery does not require a hosted Refinery account or a Refinery-operated cloud service.

Model providers still receive the content sent to their APIs. Refinery must make that boundary visible, especially when repository content, images, or video will leave the machine.

### One complete workflow before many providers

The first release supports one backend: Gemini 3.7 Flash. The product should prove that input, grounding, questions, resumability, validation, and delivery work well before adding provider choice.

### Provider-neutral product behavior

Refinery owns the concepts visible to users: cases, questions, answers, progress, outputs, and delivery. Provider-specific request and response formats remain behind backend adapters.

The initial code should preserve these boundaries without building a generalized plugin framework prematurely.

### Explicit capability handling

Backends differ. Refinery must discover or declare whether a backend supports images, native video, tool calling, structured output, and resumable conversations. It must not silently degrade a case or pretend that transformed media is equivalent to native input.

### Durable interaction

A question is persistent application state, not text that happens to appear in an agent response. Refinery records a question before presenting it, survives restarts while waiting for an answer, and applies each answer at most once.

### Least-privilege repository access

The initial repository connector is read-only. It exposes a small set of bounded tools rather than unrestricted shell access. Write access and task execution are outside the initial product scope.

## Target users

The initial user is a developer or technical product builder who:

- develops against one or more local repositories;
- begins work through conversations, transcripts, screenshots, or recordings;
- wants an agent to identify ambiguity before implementation begins;
- wants a refined prompt sent into an orchestration system such as Overlord; and
- is comfortable installing a local command-line application but should not need to configure it by editing files manually.

## Core use cases

### Refine an Overlord transcript

Overlord submits the current transcript, task metadata, attachments, and optional repository selection. Refinery asks any necessary questions and returns a prompt suitable for creating or continuing an Overlord objective.

### Ground a request in a repository

The user associates a case with a registered local repository. The agent can inspect relevant files, search the tree, and inspect Git state before deciding what information is missing or producing the final prompt.

### Refine visual feedback

The input includes screenshots, mockups, or other images. The agent incorporates visible details into its questions and output instead of relying on a separate human description.

### Refine a recording

The input includes a video such as a screen recording. A backend with native video support can inspect the video directly and use relevant observations in the refined prompt.

### Continue after clarification

An agent pauses because a product or implementation choice is ambiguous. The user answers later, possibly after Refinery has restarted, and the case continues without losing its accumulated context.

## Scope

### Included in the initial product

- Headless local service and command-line interface
- User-friendly guided setup
- Optional localhost browser interface
- Gemini 3.7 Flash through a user-supplied API key
- Text, image, and native video input
- Local-folder repository registration
- Read-only repository tools
- Structured follow-up questions and answers
- Persistent refinement cases and event history
- Structured output validation
- Direct local Overlord submission and result delivery
- Retry-safe ingress and delivery
- Local diagnostics and logs

### Not included initially

- Editing repository files
- Running arbitrary shell commands
- Executing the refined task
- Multiple simultaneous model providers
- Team accounts or a hosted Refinery control plane
- Public inbound networking to the user's machine
- General-purpose chat
- Automatic background indexing of every registered repository
- A native desktop shell
- Latch integration

## User experience

### Installation and setup

The installed product exposes a single `refinery` executable.

The first-run experience is:

```text
refinery setup
```

The guided setup should:

1. Explain what data remains local and what will be sent to the configured model provider.
2. Ask for a Gemini API key and store it in the operating system credential store.
3. Test the provider connection without displaying the secret.
4. Offer to register the current directory as a repository.
5. Detect or configure the local Overlord connection.
6. Install and start the local background service if requested.
7. Run a final health check and show the commands needed to reopen configuration.

Users can subsequently manage the installation with commands such as:

```text
refinery open
refinery status
refinery doctor
refinery repository add .
refinery repository list
refinery provider configure gemini
refinery service start
refinery service stop
```

`refinery open` launches a browser against a loopback-only local interface. It provides configuration, repository management, case history, pending questions, logs, and health information without introducing a native desktop runtime. Every important operation also has a CLI equivalent.

### Case experience

A case shows:

- its source and destination;
- the original input and attachments;
- the selected repository;
- current state;
- agent progress summaries;
- outstanding and answered questions;
- the refined output;
- delivery status and retry information; and
- a bounded diagnostic history.

The user may cancel a running case, answer a pending question, retry a failed delivery, or copy the final prompt locally.

## Refinement lifecycle

A case moves through an explicit state machine:

```text
received
  -> preparing
  -> running
  -> awaiting_answer
  -> running
  -> validating
  -> ready
  -> delivering
  -> completed
```

Terminal states and side paths are:

```text
failed
cancelled
```

The transition from `awaiting_answer` back to `running` can occur more than once. A case cannot enter `ready` while it has an unresolved required question.

Each transition is recorded as an append-only case event in addition to the case's current snapshot. This makes restart recovery, debugging, and destination synchronization understandable without requiring full event sourcing.

## Input contract

Refinery accepts a versioned `RefinementRequest` containing:

```text
request_id             stable source identifier
idempotency_key        retry-safe submission key
source                 submitting system and instance
transcript             ordered messages with roles and timestamps
task_hint              optional short description of the intended task
attachments            text, image, video, or file references
repository             optional registered repository identifier
destination            where the completed result should be sent
metadata               bounded source-specific correlation data
```

Attachments are imported into a case-owned local media store. The request records their media type, size, digest, source name, and provider-upload state. Refinery verifies size and type before accepting them.

The same `idempotency_key` and equivalent request content return the existing case. Reusing the key with different content is a conflict.

## Repository connector

Refinery registers repositories by canonical local path. Registration records a stable identifier, display name, path, and policy settings.

The first connector exposes only:

- `list_files`: list a bounded subtree while respecting ignore rules;
- `read_file`: read a bounded text range from a file;
- `search_text`: search paths and contents with result limits;
- `git_status`: inspect current repository status; and
- `git_diff`: inspect a bounded diff for a selected scope.

Every tool call is constrained to the registered canonical root. Symlinks and path traversal must not escape that root. Binary files are rejected by text tools, secrets and ignored files may be excluded by policy, and all results have byte and item limits.

The connector does not build a persistent semantic index initially. Direct search keeps the first release predictable, current, and easy to secure. Indexing can be considered later if measured cases demonstrate a performance or retrieval-quality need.

## Questions and answers

Refinery defines its own interaction types:

```text
QuestionRequest
  id
  case_id
  prompt
  questions[]
  created_at
  status

Question
  id
  label
  description
  response_type     free_text | single_choice | multiple_choice
  choices[]
  required

Answer
  question_id
  value
  answered_at
  answered_by
```

For Gemini, the agent receives an `ask_user` function with this schema. When Gemini calls it, Refinery:

1. validates and persists the request;
2. marks the case `awaiting_answer`;
3. forwards the question to the source interface and local UI;
4. waits without holding an HTTP request or process lock;
5. validates and stores the answer;
6. supplies the answer as the function result; and
7. resumes the same logical case.

Only one question request may be outstanding for a case initially. A request may contain a small related set of questions, but the agent should prefer the minimum number needed to continue.

If the source cannot render a question type, it may fall back to a plain form in Refinery's local interface. Terminal input is never the primary interaction surface.

## Output contract

The agent must produce a schema-valid `RefinedPrompt`:

```text
title                   concise task title
prompt                  self-contained instruction for the destination agent
objective               intended outcome
context                 relevant transcript, repository, and media findings
requirements            explicit functional requirements
constraints             technical, product, and operational boundaries
acceptance_criteria      observable conditions for completion
references              relevant local paths and attachment references
assumptions              assumptions made during refinement
unresolved_questions    anything intentionally left unresolved
```

The final `prompt` must stand on its own. A destination should not need access to Refinery's internal conversation to understand the work.

Refinery validates the complete object, verifies that required questions are resolved, and can run deterministic quality checks such as non-empty acceptance criteria, valid repository paths, bounded size, and absence of internal provider markup.

## System architecture

Refinery is one local Rust application with a modular internal design:

```text
Sources
  Overlord Local
  Local UI / CLI
        |
        v
Loopback API
        |
        v
Case Orchestrator ---- SQLite
   |        |             |
   |        |             +-- cases, events, questions, deliveries
   |        |
   |        +-- Repository tools
   |        +-- Local media store
   |
   v
Agent Backend
  Stage 1: Gemini
  Stage 2: Codex App Server
  Stage 3: local/custom inference
        |
        v
Destination adapters
  Overlord
  Local export
```

There is one daemon, one SQLite database, and one local data directory. Long-running work is represented by persisted jobs and state transitions rather than a separate queue service.

### Agent backend boundary

The central extension point is an `AgentBackend`, not merely a model client:

```rust
trait AgentBackend {
    async fn capabilities(&self) -> Result<BackendCapabilities>;
    async fn start(&self, request: AgentRequest) -> Result<BackendRun>;
    async fn answer(&self, run: RunId, answer: AnswerSet) -> Result<()>;
    async fn cancel(&self, run: RunId) -> Result<()>;
}
```

This name accounts for the fact that Gemini is initially driven through a Refinery-owned tool loop, while Codex later arrives as an existing agent runtime.

Capabilities include:

```text
text_input
image_input
native_video_input
tool_calling
structured_output
resumable_conversation
repository_runtime
```

The product selects or rejects a backend before starting a case when the required capabilities are unavailable.

### Job execution

The daemon claims runnable persisted jobs with leases. A lease expiry makes interrupted work eligible for recovery. Provider requests, answer application, and destination delivery use stable operation identifiers so retries do not duplicate externally visible work.

The daemon may execute several independent cases concurrently, but only one operation may mutate a given case at a time.

### Local API

The local API binds to loopback by default and uses a locally generated bearer token. Initial routes include:

```text
POST /v1/refinements
GET  /v1/refinements/{id}
POST /v1/refinements/{id}/answers
POST /v1/refinements/{id}/cancel
POST /v1/refinements/{id}/deliveries/retry
GET  /v1/refinements/{id}/events
GET  /v1/repositories
POST /v1/repositories
GET  /v1/health
```

Server-sent events are sufficient for local progress and question updates. WebSockets are unnecessary until a concrete bidirectional real-time requirement appears.

## Recommended implementation stack

- Rust stable edition
- Tokio for the asynchronous runtime
- Axum for the loopback HTTP API and local UI server
- Clap for CLI commands and guided setup
- Serde for internal and wire serialization
- Schemars for generated JSON Schemas
- SQLx with SQLite for durable state and migrations
- Reqwest for provider and destination HTTP clients
- Tracing with structured local logs
- An operating-system credential-store integration for API keys and tokens
- The `ignore` crate for repository traversal consistent with ignore files
- A small bundled web interface served by the Rust process

The browser interface should use the least complex frontend that provides accessible forms, case updates, and configuration. It is compiled to static assets and embedded into the binary; it is not a separately deployed application.

## Repository organization

Begin with one application crate and strong module boundaries:

```text
Cargo.toml
src/
  main.rs
  app.rs
  cli/
  config/
  domain/
  cases/
  agent/
    mod.rs
    gemini.rs
  interactions/
  repositories/
  media/
  storage/
  jobs/
  api/
  integrations/
    overlord.rs
  diagnostics/
web/
  src/
  static/
migrations/
schemas/
tests/
  fixtures/
  integration/
planning/
```

Do not create separate crates for every module at the outset. Extract a crate only when it has an independent consumer, testing boundary, or release lifecycle. Later provider implementations can live under `agent/` until their dependency trees or build requirements justify separate crates.

Generated public schemas are committed under `schemas/` so Overlord and future clients can validate integrations without importing Rust code.

## Data model

The initial SQLite schema contains:

- `cases`: current case state, source, destination, repository, and timestamps;
- `case_inputs`: transcript and normalized input metadata;
- `attachments`: local media metadata and provider upload state;
- `case_events`: append-only lifecycle and progress events;
- `backend_runs`: backend identity, provider conversation state, and usage metadata;
- `question_requests`: persisted question groups and status;
- `answers`: validated user responses;
- `outputs`: versioned refined outputs and validation status;
- `deliveries`: attempts, idempotency keys, response metadata, and retry state;
- `repositories`: canonical roots and access policy;
- `jobs`: durable runnable work, leases, attempts, and scheduled retry time; and
- `settings`: non-secret application settings.

Secrets are referenced from SQLite but stored in the operating system credential store. Attachment bytes live in a case-scoped data directory, not in SQLite.

## Overlord integration

### Local Overlord

An Overlord instance on the same machine sends a versioned request directly to Refinery's loopback API. The request includes stable source and idempotency identifiers. Refinery sends questions and final results back through an authenticated local Overlord endpoint or a response channel established during submission.

This path requires no cloud component.

### Hosted Overlord

A hosted service cannot reliably open an inbound connection to a user's loopback service. If hosted Overlord must submit directly to local Refinery, the connection must be initiated outbound from the user's machine.

The preferred future transport is an Overlord-owned rendezvous channel using authenticated long-polling or a secure WebSocket. Overlord stores pending submissions and responses; Refinery consumes them over an outbound connection. This is infrastructure belonging to the hosted source system, not a hosted Refinery control plane.

The product must never ask users to expose Refinery directly to the public internet.

## Security and privacy

The initial security model includes:

- loopback-only HTTP binding;
- bearer authentication for local clients;
- credentials stored outside configuration files;
- strict canonical repository roots;
- read-only repository tools;
- symlink and path-traversal protection;
- bounded file, search-result, prompt, and attachment sizes;
- explicit provider-upload disclosure;
- attachment media-type validation;
- redaction of credentials from logs and errors;
- per-case audit events for repository reads, provider calls, questions, and deliveries;
- no arbitrary shell tool in the Gemini stage; and
- no public inbound listener.

Repository content is untrusted model input. Instructions found inside files, transcripts, images, or video must be treated as content rather than system authority. The model's tool access remains constrained regardless of instructions in that content.

## Stage 1: Gemini product

### Goal

Prove the complete refinement experience with Gemini 3.7 Flash as the only backend.

### Work

1. Establish the Rust application, CLI, local service, SQLite storage, and migrations.
2. Implement guided setup, credential storage, status, and diagnostics.
3. Implement the versioned input, interaction, output, and event contracts.
4. Implement repository registration and the bounded read-only tools.
5. Implement local attachment storage and Gemini image/video upload handling.
6. Implement the Gemini agent loop with repository tools and `ask_user`.
7. Persist run state and resume after answers or process restarts.
8. Implement structured output generation and deterministic validation.
9. Implement local Overlord ingress, question forwarding, and final delivery.
10. Add the local browser interface for configuration, cases, and pending questions.
11. Add fixtures, integration tests, and refinement-quality evaluations.

### Completion criteria

Stage 1 is complete when:

- installation and setup can be completed without editing a file;
- Overlord can submit a transcript with an image or video;
- Gemini can inspect a selected local repository through bounded tools;
- Gemini can ask a structured question;
- the user can answer through Overlord or the local UI;
- the case survives a restart while awaiting that answer;
- Refinery produces a schema-valid, self-contained prompt;
- the result is delivered exactly once despite safe retries; and
- repository boundary and prompt-injection tests pass.

## Stage 2: Codex with ChatGPT subscription access

### Goal

Allow a user to connect an existing ChatGPT account and run refinements through Codex without using a terminal interaction protocol.

### Approach

Refinery supervises Codex App Server as a local child process and communicates through its structured stdio protocol. The device-code login ceremony is presented through `refinery setup` and the local UI. Codex owns its credential cache and token refresh lifecycle; Refinery does not extract ChatGPT tokens for unrelated API use.

The adapter maps:

- Codex threads and turns to backend runs;
- Codex progress events to case events;
- native user-input requests to `QuestionRequest`;
- command and file approvals to explicit approval interactions;
- App Server images to Refinery attachments; and
- structured turn output to `RefinedPrompt`.

Codex's runtime can provide its own repository tools and sandbox. Refinery still controls the registered working directory and records the visible case lifecycle.

Codex does not currently provide native video input through this interface. A video case must either select a backend with native video support or use an explicit preprocessing option that produces keyframes and a transcript. The interface must label this transformation clearly.

### Completion criteria

- A user can connect and disconnect a ChatGPT account through Refinery.
- Refinery reports the authenticated plan and usable backend state.
- Codex and Gemini produce the same canonical case, question, and output events.
- A user can answer Codex questions without opening a terminal.
- Codex process crashes and restarts produce understandable recovery behavior.
- Capability routing prevents unsupported media from being silently dropped.

## Stage 3: Local models and custom inference providers

### Goal

Let users run Refinery with a local model or an inference endpoint of their choice while preserving truthful capability reporting and reliable product behavior.

### Approach

Add configurable backend profiles for:

- common local inference servers;
- OpenAI-compatible HTTP endpoints;
- explicitly supported hosted inference services; and
- custom base URLs with user-provided credentials and model identifiers.

Compatibility cannot be inferred from an endpoint shape alone. During setup, Refinery runs a backend qualification check for required behavior:

- declared modalities are accepted;
- tool calls have usable structured arguments;
- `ask_user` can suspend and resume correctly;
- repository tool results can be returned to the model;
- structured output follows the required schema; and
- context and output limits are sufficient for the configured workflow.

Profiles that fail a capability remain available only for compatible cases, with the limitation visible to the user. Refinery does not promise identical quality across providers.

### Completion criteria

- A user can configure a local or remote endpoint without changing source code.
- Secrets are stored securely and connection diagnostics are actionable.
- Backend qualification results are persisted and visible.
- Case routing respects verified capabilities.
- Provider contract tests can be run against recorded fixtures and live opt-in endpoints.

## Future connector: Latch

Latch may later be implemented as an optional external-agent backend when it offers a stable packaged client for structured conversation events, questions, answers, and outputs.

The Latch connector would let users refine through an already-configured persistent agent environment. It would translate Latch conversation operations into Refinery's canonical case lifecycle. It would not replace the Gemini, Codex, or custom-provider backends and would not redefine Refinery's question or output contracts.

No Stage 1 design should depend on Latch-specific terminal state, transcript formats, or session behavior.

## Quality strategy

### Deterministic tests

- Serialization and schema compatibility tests
- State-machine transition tests
- Idempotent ingress, answer, and delivery tests
- Crash recovery and expired-lease tests
- Repository path escape and symlink tests
- Attachment type and size validation tests
- Provider error mapping and retry classification tests
- Secret-redaction tests

### Recorded provider tests

Provider streams and tool-call sequences are recorded as sanitized fixtures. They verify adapter behavior without requiring a live API call on every test run.

### Live integration tests

Opt-in tests exercise provider authentication, media upload, tool calling, questions, structured output, and cleanup against real accounts.

### Product evaluations

A small versioned evaluation set measures whether refined prompts are:

- self-contained;
- faithful to the original request;
- grounded in supplied repository and media context;
- explicit about requirements and constraints;
- appropriately inquisitive without asking unnecessary questions; and
- usable by a destination agent without access to hidden Refinery state.

Provider expansion should not proceed solely because the adapter works technically. It should also meet an acceptable refinement-quality baseline.

## Operational model

Refinery logs structured events locally with case and operation correlation identifiers. Logs exclude prompts, attachment bodies, repository contents, and secrets by default; a user may enable bounded diagnostic capture for a specific case.

`refinery doctor` checks:

- configuration and data-directory permissions;
- database migrations and integrity;
- credential-store availability;
- provider authentication and capabilities;
- repository reachability;
- local API authentication;
- Overlord connectivity; and
- background-service status.

Backups consist of the SQLite database, configuration, and case media directory. Provider credentials remain managed by the operating system or provider runtime and require separate reauthentication after migration.

## Product success measures

The first release should measure:

- percentage of submitted cases that reach a delivered output;
- percentage of cases that ask at least one question;
- answer-to-resume reliability;
- median time from submission to first question or output;
- delivery retry and duplication rates;
- validation failure rate;
- user edits made to the refined prompt before execution; and
- evaluation quality across the maintained fixture set.

The most important qualitative signal is whether users trust the refined prompt enough to send it to an implementation agent without manually rebuilding the context.

## Immediate implementation sequence

1. Freeze the Stage 1 JSON Schemas and state-machine invariants.
2. Scaffold the Rust application and SQLite migrations.
3. Implement cases, events, jobs, and restart recovery.
4. Implement setup, credential storage, and diagnostics.
5. Implement repository registration and read-only tools.
6. Implement media import and Gemini upload lifecycle.
7. Implement the Gemini tool loop and structured questions.
8. Implement output validation.
9. Implement local Overlord ingress and delivery.
10. Add the local browser interface.
11. Build the evaluation set and run end-to-end acceptance tests.

Stage 2 work begins only after Stage 1 has demonstrated a dependable question-and-answer loop and consistently useful refined outputs.
