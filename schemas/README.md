# Generated schemas

JSON Schemas for Refinery's externally visible contracts, generated from the
Rust types in `src/domain/` with Schemars and committed here so Overlord and
future clients can validate integrations without importing Rust code.

Regenerate after any contract change:

```sh
cargo run --bin refinery-schemas -- --out schemas
```

`scripts/check-schema-drift.sh` runs the same generator into a temporary
directory and diffs the result against this one, so CI fails when a committed
schema no longer matches the type that produced it. Never hand-edit a file
here; the diff is the review.

## What is published

Only wire roots. A type that appears solely inside another contract is already
described by that contract's `definitions`, and publishing it separately would
give clients two places to look for the same rules.

| File | Contract |
| --- | --- |
| `refinement_request.json` | What a source submits to start a case |
| `question_request.json` | A group of questions the agent needs answered |
| `answer_set.json` | The answers to one question request |
| `refined_prompt.json` | The agent's validated output |
| `case_state.json` | The lifecycle states a case can be in |
| `case_event.json` | One record in a case's append-only history |
| `backend_capabilities.json` | What an agent backend can do |
| `delivery_envelope.json` | What a destination receives |
| `delivery_receipt.json` | What a destination returns |
| `api_error.json` | The uniform local API error body |
| `contract.json` | The contract version these files describe |

## Versioning

Every contract carries an explicit `schema_version`, and one `contract_version`
covers the set: the types are submitted, answered, and delivered together, so a
client that understands one version understands all of them. This build accepts
its own version and refuses any other rather than guessing at a document it may
misread.

Contracts were frozen in milestone M1. A change that alters any schema here is a
contract change: bump `CONTRACT_VERSION` in `src/domain/mod.rs`, regenerate, and
update the frozen fixtures under `tests/fixtures/contracts/` deliberately rather
than to make a test pass.
