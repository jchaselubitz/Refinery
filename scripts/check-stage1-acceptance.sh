#!/usr/bin/env bash
set -euo pipefail

# Run every executable gate once. The evidence map below then makes the nine
# product-description criteria visible without re-running the suite nine times.
cargo test
./scripts/check-schema-drift.sh
./scripts/check-interface-assets.sh
./scripts/check-recorded-fixtures.sh
./scripts/run-evaluations.sh

test_list="$(cargo test -- --list 2>/dev/null)"

require_test() {
  local criterion="$1"
  local test_name="$2"
  if ! printf '%s\n' "$test_list" | rg -F -q "$test_name: test"; then
    printf '[FAIL] %s\n       missing evidence: %s\n' "$criterion" "$test_name" >&2
    exit 1
  fi
}

pass() {
  printf '[PASS] %s\n' "$1"
}

criterion='installation and setup can be completed without editing a file'
require_test "$criterion" 'cli::setup::tests::a_clean_machine_is_configured_without_editing_a_file'
pass "$criterion"

criterion='Overlord can submit a transcript with an image or video'
require_test "$criterion" 'api::tests::ingress_is_authenticated_idempotent_and_streamable'
require_test "$criterion" 'media::a_video_is_polled_to_activation_before_it_is_reported_available'
pass "$criterion"

criterion='Gemini can inspect a selected local repository through bounded tools'
require_test "$criterion" 'agent::a_full_refinement_reads_the_repository_and_submits_an_accepted_prompt'
require_test "$criterion" 'repositories::connector::tests::files_are_text_only_and_all_text_results_are_bounded'
pass "$criterion"

criterion='Gemini can ask a structured question'
require_test "$criterion" 'agent::a_question_pauses_the_case_and_the_answer_resumes_it_after_a_restart'
pass "$criterion"

criterion='the user can answer through Overlord or the local UI'
require_test "$criterion" 'ui::every_interface_interaction_works_against_a_live_daemon'
pass "$criterion"

criterion='the case survives a restart while awaiting that answer'
require_test "$criterion" 'agent::a_question_pauses_the_case_and_the_answer_resumes_it_after_a_restart'
pass "$criterion"

criterion='Refinery produces a schema-valid, self-contained prompt'
require_test "$criterion" 'contracts::a_refined_prompt_round_trips_and_passes_every_deterministic_check'
require_test "$criterion" 'evaluations::tests::the_committed_v1_set_meets_its_baseline'
pass "$criterion"

criterion='the result is delivered exactly once despite safe retries'
require_test "$criterion" 'ui::a_safe_delivery_retry_is_applied_exactly_once'
pass "$criterion"

criterion='repository boundary and prompt-injection tests pass'
require_test "$criterion" 'repositories::connector::tests::escaping_and_swapped_symlinks_never_widen_the_root'
require_test "$criterion" 'agent::an_injection_in_a_repository_file_arrives_as_data_and_changes_no_tool'
pass "$criterion"

printf 'Stage 1 acceptance: 9 / 9 criteria passed\n'
