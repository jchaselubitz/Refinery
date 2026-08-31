# Refinery (working name) — standalone transcript-to-objective refinement service

**Status:** proposed product plan — developed under Overlord mission `coo:882`
and moved into this repository (2026-08-31); stack, repository, and direct
Overlord-ingestion decisions were refined under `coo:885`. Nothing in this
document is implemented yet.
**Supersedes:** the in-Overlord mechanism plan
(`planning/feature-plans/coo-882-refine-objective-with-ai.md` in the Overlord
repo). The accept-moment UX and grounding-skill thinking from that plan carry
forward; its execution-request/runner mechanism does not.
**Relationship to the Cooperativ family:** Latch runs local agent sessions;
Overlord orchestrates missions; Refinery turns messy human conversation into
well-formed work items for systems like Overlord.

---

## 1. Product definition

Refinery is a **lightweight service on the user's machine** that converts raw
feedback — a transcript of one or more messages, with media — into a
**grounded, well-specified work item**, by running an AI agent against the
user's local repositories.

The core loop:

```text
transcript in  →  ground against repo(s)  →  confident?
                                              ├─ yes → structured result out
                                              └─ no  → structured clarifying
                                                       questions ⇄ answers
                                                       (repeat, bounded)
result  →  mapped by a destination profile  →  delivered to a configured route
           (default: an Overlord objective in a chosen project, created via
            the user's own `ovld` CLI / MCP credentials)
```

It is a **tool acting directly for one user**. It has no accounts, no logins,
and no role-based access control of its own; its authority to create
objectives (or anything else) is exactly the user's existing credentials on
the destination side. Anyone who can talk to the service's loopback API *is*
the user, by definition of the trust model (§4).

### What it is not

- Not a hosted/multi-tenant service. One machine, one user, local state.
- Not a chat product. Conversations are bounded clarification loops with a
  terminal output, not open-ended sessions.
- Not a mission tracker. It holds a case only until the result is delivered
  or the case is discarded; the destination system owns the work item.
- Not an agent harness. It delegates agentic execution to Latch-launched
  agents or (phase 2) drives a model API with its own minimal read-only loop.

## 2. Primary use cases

1. **Overlord objective refinement.** From Overlord, send a vague
   objective/inbox capture (plus its intake provenance, e.g. a Slack thread)
   as a transcript. Answer any clarifying questions in Overlord's UI. A
   refined draft objective appears in the target project.
2. **Pre-objective intake.** A Slack (or email) integration forwards a short
   conversation to Refinery *before any Overlord entity exists*. After the
   back-and-forth, the result lands directly as a new draft
   mission/objective.
3. **Non-Overlord consumers.** Any local tool POSTs a transcript and receives
   the canonical result on its own route (or reads it back from the case),
   using the same structured-question schema to render its own Q&A UI.

## 3. Core concepts

| Concept | Meaning |
| --- | --- |
| **Case** | One refinement job: a growing transcript, its grounding runs, pending questions, and terminal outcome. Durable in local SQLite. |
| **Transcript** | Ordered list of messages. The *only* input shape. Grows over the case's life: original feedback, agent questions, user answers, guidance — all messages. |
| **Message** | `{ author, sentAt, parts[] }`. Author is a display label + kind (`user`, `agent`, `external`) — never an authorization claim. Parts are text, image, or video refs. |
| **Question** | A first-class, typed, addressable record the agent raises when not confident. Also appears in transcript order. |
| **Grounding run** | One agent execution over the current transcript + repo(s). Produces either questions or a result draft. |
| **Result** | The canonical output object (§8). Immutable once emitted. |
| **Driver** | How agents execute: `latch` (v1) or `api` (phase 2). |
| **Destination profile** | Named config mapping the canonical result onto a receiving system: `overlord_cli` (default), `mcp`, `webhook`. |
| **Repo registry** | Service-local map of repo keys → absolute paths. Callers reference repos by key; multiple repos per case are allowed. |

### Case lifecycle

```text
open → grounding → awaiting_answers → grounding → … → confirming?
                                                        ├→ delivered
                                                        └→ discarded
any state → failed | expired | discarded
```

- `confirming` is optional per case: the final "here is the result — ship
  it?" is expressed as one more structured question (a `confirmation` type),
  so approval needs no separate machinery. With confirmation off, a
  confident result delivers immediately.
- Bounded rounds (default max 5 grounding runs per case) and a per-case
  deadline prevent zombie cases.

## 4. Trust and security model (single-user, by decision)

Per PM decision: **no user logins, no RBAC**. The service is a personal tool.

- **Loopback only.** The HTTP API binds `127.0.0.1` exclusively (same rule as
  Latch's gateway). The service never listens on a public interface; remote
  intake is pull-based (§9).
- **One local bearer token**, generated at first run, stored `0600` in the
  service home (e.g. `~/.refinery/serve.token`), required on every request.
  This is not user auth — it is defense against *other* local principals:
  browsers can POST to localhost, so an unauthenticated loopback API is a
  drive-by CSRF target. CORS is denied entirely; the token never appears in
  URLs.
- **Secrets:** model/agent API keys and any webhook credentials live in the
  OS keychain where available. A platform without a supported keychain must
  use an explicitly configured environment variable or `0600` secret file
  with a warning; do not claim protection from home-grown encryption whose
  key is stored beside the ciphertext. Secrets never appear in logs,
  transcripts, results, or error messages.
- **Destination authority = the user's own credentials.** The default
  Overlord profile shells out to `ovld` (or calls an MCP server) as the
  logged-in user; Refinery stores no Overlord token at all.
- **Repo access is read-only by contract** (briefing rules + skill), with the
  same honesty as the earlier plan: v1 has no hard sandbox; the driver runs
  agents with no write mandate, and a future hardening can use read-only
  worktrees/containers.
- **Media retention:** stored under the service home with a size cap and a
  retention window; purged when a case reaches a terminal state (configurable
  grace period).

## 5. Service architecture

```text
┌────────────────────────────── Refinery (localhost daemon) ─────────────────────────────┐
│  HTTP API (127.0.0.1, bearer)        SQLite case store        media store (~/.refinery) │
│        │                                                                               │
│  Case engine ── transcript, rounds, questions, deadlines, round caps                   │
│        │                                                                               │
│  Grounding runner ── briefing composer + grounding skill                               │
│        ├── Latch driver (v1): latch create → v2 Conversation Hub → result collection   │
│        └── API driver (v2): provider-agnostic model call + built-in read-only tools    │
│        │                                                                               │
│  Repo registry ── name → path, read-only                                               │
│        │                                                                               │
│  Delivery engine ── destination profiles: overlord_cli │ mcp │ webhook                 │
│  Intake connectors (v2) ── outbound pollers (Overlord queue, Slack relay, …)           │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

Implementation notes: single small TypeScript service (matching the family's
stack), SQLite via the same patterns Overlord Local uses, and no external
infrastructure dependencies beyond the chosen driver's needs. Ship as a CLI
with a foreground `serve` mode first; OS-service installation can copy
Overlord's runner-service pattern later.

### 5.1 Recommended stack

Build a **modular monolith**, not a set of services. There is one executable,
one SQLite database, and one local state directory. HTTP handling and
grounding work may run in separate in-process components, but every durable
transition goes through SQLite so a crash or restart does not lose a case.

| Layer | Choice | Why |
| --- | --- | --- |
| Runtime/tooling | Node 24 LTS, strict TypeScript, ESM, Yarn 4 workspaces | Matches current Overlord development and packaging knowledge; Node gives the cleanest path to the `ovld` and Latch CLIs. |
| HTTP | Express 5, bound explicitly to `127.0.0.1`; native SSE responses | Keeps Overlord's familiar backend/test model while starting the greenfield service on the current major. The small API does not justify a second web framework. |
| Contract validation | TypeBox 1 schemas + its schema compiler; generated JSON Schema committed as release artifacts | One source provides runtime validation, TypeScript types, and a language-neutral contract for Overlord and future clients without a second validation dependency. |
| Persistence | `better-sqlite3` + Kysely, WAL mode, checked-in forward-only migrations | Local durability, transactions, and typed queries without a database server. This is already a known native-packaging problem in the family. |
| Durable work | SQLite job/outbox tables plus an in-process worker with leases | Grounding and delivery survive restarts without Redis, a broker, or a second daemon. Global and per-case concurrency are enforced transactionally. |
| Logging | Structured JSON logs with automatic redaction; pretty development transport only | Makes run/case correlation useful while keeping transcript, media, bearer tokens, and model secrets out of logs. |
| Tests/build | `node:test`, temporary per-test service homes, HTTP contract tests, esbuild bundle with `better-sqlite3` external | Mirrors the family's lightweight test/build style and exercises real migrations and restart behavior. |

Do not add a web UI, Postgres adapter, Redis, container requirement, plugin
framework, or generic workflow engine to v1. Overlord is the primary UI; the
Refinery CLI is the escape hatch and diagnostic surface.

### 5.2 Process and persistence boundaries

The executable has four composition-root components:

```text
HTTP adapter ──► case application service ──► SQLite repositories
                         │                         ▲
                         ▼                         │
                  durable job queue ──► grounding/delivery worker
```

- A route may validate and persist intent, but it does not wait for an agent
  run. `POST /v1/cases` returns `202` once the case and initial job commit.
- The worker leases a job, invokes a driver/destination port, then commits the
  resulting domain event and next job in one transaction. External calls are
  necessarily outside the transaction and use stable idempotency keys.
- The outbox feeds SSE subscribers and optional loopback callbacks. SSE is a
  convenience over durable events, not the only place an event exists.
- SQLite stores metadata and textual parts. Binary media lives under the
  service home and is referenced by content digest; database rows own its
  lifecycle.

### 5.3 Repository organization

Use a workspace from day one because Overlord needs a small shared client,
but keep the number of packages bounded and ship only one application:

```text
apps/
  refinery/                 CLI, daemon, composition root, packaging
    src/
      http/                 loopback routes, auth, SSE
      persistence/          SQLite migrations and repositories
      workers/              durable jobs and outbox dispatch
      drivers/latch/        discovery, launch, conversation transport
      destinations/         ovld and webhook adapters
packages/
  contract/                 TypeBox source schemas + generated JSON Schema
  client/                   dependency-light typed HTTP client used by Overlord
  core/                     cases, runs, questions, results, delivery use cases
skills/
  grounding/                versioned agent instruction pack and fixtures
test/
  contract/  integration/  fixtures/
docs/
  adr/                      decisions that alter trust or process boundaries
```

`core` owns ports, never CLI/process/HTTP imports. Adapters depend inward on
those ports. `contract` contains only wire types and validators — not database
models. `client` is safe to consume from Overlord's backend/desktop code and
does not know how to read the local bearer token; the desktop/backend adapter
injects that token. Do not make Overlord import Refinery source by relative
path. Publish/version `contract` and `client` together (a private registry or
release tarballs are sufficient initially).

The package boundaries are architectural, not deployment boundaries. If the
workspace overhead feels high during the first spike, `core` can also begin
under `apps/refinery/src/`; `contract` and `client` must remain independently
versionable because Overlord consumes them. Add an adapter package only when
another application genuinely needs to install that adapter separately.

### 5.4 Durable data model (outline)

Keep the relational model explicit rather than storing whole cases as JSON:

| Table | Role |
| --- | --- |
| `cases` | Lifecycle, source/correlation, selected driver/destination, round/deadline policy, transcript digest |
| `messages`, `message_parts` | Immutable transcript order and typed text/media parts; caller message ids enforce append idempotency |
| `media` | Digest, MIME/size, local path, retention state; no binary blobs in SQLite |
| `grounding_runs` | Attempt/round, driver session id, lease/timing, terminal outcome and bounded error |
| `questions`, `answers` | Versioned structured Q&A with stable ids and transcript linkage |
| `results` | Immutable canonical result JSON plus indexed case/version/confidence fields |
| `deliveries` | Destination, idempotency key, attempts, receipt/external id, last bounded error |
| `jobs` | Leased durable commands (`ground`, `deliver`, `expire`, `purge_media`, `emit_callback`) |
| `case_events` | Ordered durable event/outbox stream used by polling, SSE, callbacks, and relay connectors |
| `repos`, `profiles` | Local registry/config; secret values are keychain references only |

The first migration should establish foreign keys, uniqueness for source
idempotency and one active run per case, and the event sequence invariant.
Store schema-versioned external payloads as JSON only at the edges (`result`,
question options, destination receipts); keep lifecycle fields queryable.

## 6. HTTP API (v1)

All routes loopback + bearer. All bodies JSON; media uploaded as multipart or
by local path reference.

```text
POST   /v1/cases                     idempotently submit a case envelope
GET    /v1/cases/:id                 full case state (transcript, questions, status, result)
GET    /v1/cases                     list (status filter, bounded)
POST   /v1/cases/:id/messages        append messages (answers-as-prose, guidance, more feedback)
POST   /v1/cases/:id/answers         structured answers: [{ questionId, value }]
POST   /v1/cases/:id/run             trigger a grounding run (or rely on auto-run options)
POST   /v1/cases/:id/discard         terminal; purges media per retention policy
GET    /v1/cases/:id/events          SSE stream: question_raised, run_started, result_ready,
                                     delivered, failed (poll GET /cases/:id is always sufficient)
GET    /v1/results/:id               canonical result by id
POST   /v1/results/:id/deliver       re-deliver / deliver to an alternate profile

GET/PUT /v1/config/repos             repo registry
GET/PUT /v1/config/drivers           driver configs (api keys by keychain ref only)
GET/PUT /v1/config/destinations      destination profiles
GET    /v1/health                    version, driver availability (latch discovery), queue depth
```

The direct-ingestion request is the product boundary between Overlord and
Refinery:

```jsonc
{
  "schemaVersion": 1,
  "source": {
    "kind": "overlord",
    "externalId": "objective:<uuid>",
    "label": "coo:885.4mjm"
  },
  "transcript": { "messages": [/* canonical messages and media refs */] },
  "repoKeys": ["refinery", "overlord"],
  "options": {
    "driverId": "latch-default",
    "destinationId": "overlord-refinery-project",
    "requireConfirmation": true
  },
  "interaction": {
    "mode": "poll" | "loopback_callback",
    "callback": { "url": "http://127.0.0.1:…", "token": "…" }
  }
}
```

- Overlord sends `Authorization: Bearer …` and an `Idempotency-Key` that is
  stable for the user action. A repeated submission returns the original
  `caseId`; it does not start a second refinement.
- `source.externalId` is provenance/correlation, not authority. Refinery
  copies the submitted transcript into the case; later edits in Overlord do
  not silently rewrite the historical input.
- Every submitted message also has a caller-stable message id. If Overlord
  forwards later conversation, append is idempotent on `(source,
  sourceMessageId)`.
- Callback URLs are loopback-only and their tokens are encrypted/redacted.
  Cloud interaction never asks the laptop daemon to callback to an arbitrary
  URL.
- The response is `202 { caseId, status, eventsUrl }`. Overlord can poll, use
  SSE, or accept callbacks; all three observe the same durable case events.

`options` on case creation: `{ driverId?, destinationId?, autoRun?: true,
requireConfirmation?: boolean, maxRounds?, deadlineMinutes? }`. The optional
`interaction.callback` lets a submitting client receive `question_raised` /
`result_ready` pushes at its own loopback route instead of polling.

Everything is versioned under `/v1`; the question and result schemas carry
their own `schemaVersion` so client renderers can evolve independently.

## 7. Structured questions — the interoperability contract

Questions are the surface other software builds UI against, so the schema is
deliberately small and closed (additions bump `schemaVersion`):

```jsonc
{
  "schemaVersion": 1,
  "id": "q_8f2c",
  "caseId": "case_…",
  "type": "free_text" | "single_choice" | "multi_choice" | "confirmation",
  "prompt": "Which screen does 'the export button is missing' refer to?",
  "context": "The phrase matches two surfaces in the repo.",   // why the agent asks
  "options": [                                                  // choice types only
    { "id": "a", "label": "Review screen", "detail": "webapp/src/review/…" },
    { "id": "b", "label": "Report detail", "detail": "webapp/src/reports/…" }
  ],
  "subject": {                                                  // optional grounding anchor
    "kind": "file" | "ui_element" | "media",
    "repo": "overlord", "path": "webapp/src/review/Toolbar.tsx",
    "mediaId": null
  },
  "required": true,
  "allowFreeTextWithChoice": true          // "Other: …" escape hatch on choice types
}
```

Answers: `{ questionId, value }` where value is a string, option id, or
option-id array; every answer is also appended to the transcript as a user
message so the next grounding run sees Q&A in conversational order. The
`confirmation` type with the pending result attached is how final approval is
asked (§3); answering it negatively with free text is a revision request and
starts another round.

Design rules: the agent must batch questions per round (one interruption, not
a drip), mark only genuinely blocking ones `required`, and never exceed a
per-round question cap (default 5).

## 8. Canonical result

One stable shape for every destination; profiles map it, they don't replace
it:

```jsonc
{
  "schemaVersion": 1,
  "caseId": "case_…",
  "title": "Fix missing export button on the review screen",
  "prompt": "…the refined, codebase-accurate work instruction…",
  "summary": "Narrowed the report to the review toolbar; named files and constraints.",
  "rationale": "…",
  "assumptions": ["Only CSV export is in scope"],
  "confidence": "high" | "medium" | "low",
  "codeReferences": [{ "repo": "overlord", "path": "webapp/src/review/Toolbar.tsx", "reason": "…" }],
  "messageReferences": [{ "messageId": "m_3", "reason": "constraint came from Alice's correction" }],
  "suggestedSplit": [{ "title": "…", "prompt": "…" }],          // optional
  "transcriptDigest": "sha256:…",                                // provenance pin
  "media": [{ "mediaId": "…", "kind": "image", "note": "annotated screenshot" }]
}
```

## 9. Intake paths and cloud boundary

**The local product does not require any cloud component.** If the action is
initiated in Overlord Local/Desktop, its trusted local backend posts the
envelope in §6 directly to Refinery's loopback API. The browser SPA never
receives the Refinery token and never calls localhost itself.

Cloud becomes necessary only when the initiating Overlord process is hosted.
A hosted process cannot call `127.0.0.1` on a user's laptop, and opening or
tunnelling the Refinery daemon would violate the trust model. In that mode,
the minimum cloud component is an **Overlord-owned rendezvous relay**, not a
hosted Refinery deployment:

```text
Overlord Cloud ── enqueue transcript ──► Overlord refinement relay
                                                ▲            │
                              answers/results   │            │ outbound claim
                                                │            ▼
                                         local Refinery daemon
```

The relay should reuse Overlord's existing backend, database, authentication,
runner-style leasing, and attachment storage:

1. Overlord commits an intake envelope with a stable idempotency key and an
   expiry/cancellation state.
2. Refinery authenticates as the user/device and long-polls outbound to claim
   work. It converts the envelope into the exact same `SubmitCase` command the
   loopback route uses.
3. Refinery posts durable case events (questions, status, confirmation, result
   reference) back to the relay. Overlord renders them and appends answers.
4. Refinery receives answers on its next long-poll and advances the case.
   Delivery still uses the configured destination profile and existing
   Overlord create/add-objective surfaces.

This likely needs two small Overlord tables (relay items and ordered relay
events) plus existing object storage for media. It does **not** need Redis, a
new queue vendor, a separate public service, inbound laptop networking, or a
second Refinery database. If media already exists in Overlord storage, the
envelope should carry short-lived download references rather than duplicating
bytes in relay rows.

Therefore the product modes are explicit:

| Initiator | Path | New cloud infrastructure? |
| --- | --- | --- |
| Overlord Local/Desktop | Direct loopback push | No |
| Local script/editor | Direct loopback push | No |
| Overlord Cloud/web | Overlord relay + Refinery outbound long-poll | Yes, but only inside Overlord's existing cloud stack |
| Slack/email before an Overlord entity exists | Source integration → Overlord relay → Refinery | Yes; phase 2 |

The loopback API and relay transport share the versioned envelope/event
schemas from `packages/contract`; they are adapters over one case engine, not
two ingestion implementations.

## 10. Execution drivers

The decisive design fact: the two options differ in **who owns the agentic
loop**.

### 10.1 Latch driver (v1 default)

Latch launches a full local coding agent that brings its own loop and repo
tools; Refinery briefs it and collects the output.

- Discovery per Latch's public CLI (`latch capabilities --json`), with the
  same found / not-installed / incompatible states Overlord models.
- Launch via `latch create --manifest-file - --json` in (one of) the case's
  registered repo paths; record the session id on the grounding run.
- The briefing (a file in a per-run scratch dir) carries: the full transcript
  (with media file paths), repo registry entries for the case, prior rounds,
  the grounding skill (§11), the question/result schemas, and the exact
  loopback callback the agent must use to respond:
  `POST /v1/cases/:id/agent-response` with a one-time run token minted for
  that run (scoped to the case, expires with the run — the general bearer
  token is *not* given to the agent).
- **Live rounds:** because Latch's v2 Conversation Hub lets Refinery send
  messages into a running session, a session can be kept alive (bounded
  keep-alive window, default 10 minutes) while `awaiting_answers`; answers are
  relayed in-session instead of relaunching. If the window lapses, the next
  round is a fresh launch with the grown transcript — both cadences produce
  identical case state.
- Run ends when the agent posts questions or a result draft, or on timeout →
  `failed`/round retry.

### 10.2 API driver (phase 2)

For "call an arbitrary agent API with an API key": a raw model API cannot
explore a local repo, so **Refinery owns the loop** — a provider-agnostic
chat/tool-call client plus a deliberately minimal, read-only tool belt:

```text
read_file(repo, path, range?)   list_dir(repo, path)   grep(repo, pattern, glob?)
git_log(repo, path?, limit)     view_media(mediaId)
```

- Provider adapters kept thin (Anthropic first; the interface is
  OpenAI-compatible-friendly). API keys by keychain reference only.
- Multimodal: images pass through to vision-capable models; video handled by
  key-frame extraction in v2.x (§13).
- Token/cost budget per run (configurable), hard tool-call cap, no
  write-capable tools at all — the API driver is sandboxed by construction,
  which the Latch driver is not.
- The model's final structured output is validated against the same
  question/result schemas; the loop retries schema violations once.

### 10.3 Driver selection

Per destination-profile default, overridable per case (`driverId`). `/health`
reports which drivers are currently usable (Latch discovery state, key
presence) so clients can grey out options.

## 11. The grounding skill

A built-in, versioned instruction pack included in every briefing (and, for
the Latch driver, installable as a harness skill so any local agent behaves
identically). Staged behavior:

1. **Identify referents first.** Before proposing anything, determine exactly
   which features, screens, and UI elements the transcript refers to: search
   UI strings, routes, component and command names; match screenshots to
   components; follow the code until the referent is concrete.
2. **Read the conversation as a conversation.** Multi-party, chronological,
   possibly contradictory: the latest authoritative statement controls;
   corrections override originals; unresolved contradictions become
   questions, never guesses.
3. **Gate on confidence.** Confident → produce a result draft. Not confident
   → produce the smallest set of batched, typed questions that would make you
   confident (respect the round cap; prefer choice questions anchored to
   concrete files/elements over open prompts).
4. **Ground every claim.** Each requirement in the result cites a message
   (`messageReferences`) and, where applicable, code (`codeReferences`).
   State assumptions explicitly rather than silently resolving ambiguity.
5. **Never write.** No file modification, no commits, no network calls beyond
   the provided response endpoint. Respond exactly once per run.

## 12. Destination profiles and Overlord integration

### 12.1 Profile kinds

| Kind | Behavior |
| --- | --- |
| `overlord_cli` (default) | Executes the user's own `ovld` binary: `ovld protocol create --agent refinery --project-id <profile.projectId> --objectives-json …` (new mission) or `ovld protocol add-objectives --mission-id …` (append), built from the canonical result. Draft state by default — the accept/edit moment stays in Overlord's existing UI. No stored credentials; `ovld`'s auth is the auth. |
| `mcp` | Calls a configured MCP server tool (e.g. hosted `overlord_create_mission`) as the user, for setups where the CLI is absent. |
| `webhook` | POSTs the canonical result (optionally through a JSON mapping template) to a configured URL with a configured auth header. The generic escape hatch for any other software. |

A profile = `{ id, kind, config, defaultDriverId?, mappingTemplate? }`.
Delivery is at-least-once with an idempotency key (`caseId` + result id) so a
retried delivery never duplicates an objective; `overlord_cli` relies on the
protocol layer's existing idempotency scopes.

### 12.2 What stays in the Overlord repo (thin client)

Specified separately in the **Overlord repo**:
`planning/feature-plans/coo-882-overlord-refinery-integration.md` — the
"Refine" send action, question/confirmation rendering, provenance, settings,
and the phase-2 Cloud intake queue. Summary of the boundary: Overlord builds
no refinement-domain tables, no runner changes, and no new protocol verbs in
v1; Refinery creates objectives through the same authenticated create
surfaces as every agent (identifier `refinery`, draft state). The phase-2
cloud relay adds transport/lease tables in Overlord, but case state and the
refinement engine remain exclusively in Refinery.

## 13. Limits and failure modes

| Concern | Behavior |
| --- | --- |
| Transcript bounds | Caps on message count, total characters, media count/size; over-limit submissions are rejected with explicit limits, never silently truncated. |
| Rounds/deadlines | Max rounds and case deadline → `expired` with everything gathered so far preserved and readable. |
| Latch unavailable | `/health` + case failure with the named discovery state; API driver (when configured) offered as fallback. |
| Agent invalid response | Schema validation at the response endpoint; one retry prompt per run, then round failure. |
| Delivery failure | Result retained; retriable via `POST /v1/results/:id/deliver`; at-least-once with idempotency. |
| Video | v1 stores and passes through file paths (Latch-driver agents may handle them natively); API driver defers video to key-frame extraction in v2.x. Never a blocker for case creation. |
| Concurrent runs | One active grounding run per case; global concurrency cap (default 2) since runs are heavy. |

## 14. MVP and phases

**MVP (v1):**
versioned `contract` + typed `client` · idempotent direct-ingestion envelope ·
loopback HTTP API + bearer token · SQLite case/job/event store · transcript
with text + images · typed questions with polling and `GET /events` SSE ·
Latch driver (fresh-launch cadence; keep-alive if cheap) · grounding skill v1
· `overlord_cli` destination profile · repo registry · `refinery serve` +
minimal CLI (`refinery case create --transcript-file -`, `case show`, `case
answer`). The first vertical acceptance test is: Overlord Local submits a
transcript, Refinery survives a restart during grounding, a question round is
answered, and exactly one draft objective is delivered despite a forced
delivery retry.

**v1.x:** case-creation callbacks, `webhook` and `mcp` profiles,
confirmation-question flow polish, media retention policies, OS-service
install.

**v2:** API driver with the read-only tool loop and provider adapters ·
outbound pull connectors (Overlord Cloud queue, then Slack relay with
in-thread Q&A) · video key-frames · suggestedSplit delivery as multiple
objectives.

**Later:** read-only worktree/container sandboxing for the Latch driver ·
per-repo "how we write objectives here" preambles · additional destination
profiles (GitHub Issues, Linear).

## 15. Open questions

1. **Name.** "Refinery" is a working name; settle it before package publishing
   or product launch.
2. **Repo-hint trust:** when a caller passes absolute paths instead of
   registry keys, accept (it's the user's machine) or require registration
   first? Leaning: registry keys only, one-call registration, to keep case
   records portable and the agent's world enumerable.
3. **Keep-alive default for the Latch driver** — always try, or opt-in per
   case? (Cost of an idle session vs. latency of relaunch.)
4. **Should `confirmation` answers allow inline edits** (user edits the
   prompt text in the confirmation UI, service delivers the edited version)
   — likely yes; it mirrors Overlord's edit-then-accept and removes a round.
5. **Multi-result cases:** does one case ever deliver to two destinations
   (e.g. objective + a notification webhook)? Leaning yes via
   `POST /results/:id/deliver` rather than multi-destination options.
6. **Ever run the refinement engine in the cloud?** Draft objective
   `coo:882.zc5s` sketches GitHub-repo access + a structured model call run
   server-side. Default answer: no — first build the local API driver and the
   Overlord relay. A hosted engine would introduce repository authorization,
   tenant isolation, hosted secret/cost controls, and a second persistence
   model; treat it as a distinct deployment product and reconcile that
   objective with this plan before proceeding.
