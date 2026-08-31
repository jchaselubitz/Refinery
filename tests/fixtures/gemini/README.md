# Recorded Gemini Files API fixtures

Sanitized response bodies from the Gemini Files API, served by the fake
provider in `tests/integration/media.rs` so the upload lifecycle is exercised
without a network or a credential.

Two placeholders are substituted at serve time: `__NAME__` becomes the file
resource name the fake assigns to that upload, and `__EXPIRES__` becomes an
expiry the test controls, which is what lets the re-upload test observe an
expired provider copy without waiting two days.

Nothing here contains a key, a token, or a project identifier: recorded bodies
are scrubbed before they are committed. Do not copy a raw capture into this
directory. Record through the gated sanitizer:

```sh
REFINERY_RECORD_FIXTURES=1 cargo run --bin refinery-fixtures -- \
  record /tmp/raw-provider-response.json \
  tests/fixtures/gemini/generate_new_case.json
```

Use `-` as the input to read from stdin. The recorder registers known live
credential environment variables, structurally removes credential fields, and
scans every string (including URLs and provider error messages) before opening
the output. Without `REFINERY_RECORD_FIXTURES=1` it refuses to write. Run
`./scripts/check-recorded-fixtures.sh` to verify every committed response is
unchanged by the sanitizer.

## Recorded `generateContent` fixtures

The `generate_*.json` bodies are Gemini responses for the agent tool loop,
served in order by the fake provider in `tests/integration/agent.rs`. Each one
is a single turn: a tool call, a question, a submission, or a bare text reply.
A test scripts a conversation by naming the sequence of turns the provider
should return, which is what lets a full refinement, a question round trip, and
a malformed-output retry all run with no network and no credential.
