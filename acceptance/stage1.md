# Stage 1 acceptance

Run `./scripts/check-stage1-acceptance.sh`. It executes the complete Rust test
suite, schema drift, embedded-interface checks, recorded-fixture sanitization,
and the versioned quality evaluation, then prints one evidence-backed result
for each criterion below.

The criteria are copied verbatim from the product description:

1. installation and setup can be completed without editing a file;
2. Overlord can submit a transcript with an image or video;
3. Gemini can inspect a selected local repository through bounded tools;
4. Gemini can ask a structured question;
5. the user can answer through Overlord or the local UI;
6. the case survives a restart while awaiting that answer;
7. Refinery produces a schema-valid, self-contained prompt;
8. the result is delivered exactly once despite safe retries; and
9. repository boundary and prompt-injection tests pass.

The script fails if any gate fails or if a named evidence test disappears. A
criterion therefore cannot stay green because a refactor silently deleted the
test that justified it.
