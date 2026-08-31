# Refinement quality evaluations

`v1/` is the frozen Stage 1 baseline. Every case contains the original
transcript, a tiny repository fixture, a candidate `RefinedPrompt`, and a
reviewable rubric for five product qualities: self-containment, faithfulness,
grounding, explicitness, and question economy.

Run it with:

```sh
cargo run --bin refinery-eval -- --set evaluations/v1
```

The scorer is deterministic. It validates the output contract, confirms every
faithfulness fact exists in the transcript, confirms every expected repository
reference resolves inside the case fixture, and reports each dimension per
case. A live or recorded provider run can replace a candidate output without
changing the rubric, which makes quality regressions visible independently of
adapter correctness.

Changing an existing rubric changes the meaning of its score. Add a new set
version instead; keep old versions runnable so provider changes can be compared
against the same baseline.
