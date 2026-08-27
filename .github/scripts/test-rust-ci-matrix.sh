#!/usr/bin/env bash
# shellcheck disable=SC2016 # GitHub expressions below are literal patterns.
set -euo pipefail

workflow="${1:-.github/workflows/ci.yaml}"

require_text() {
  local expected="$1"
  local description="$2"

  if ! grep -Fq -- "$expected" "$workflow"; then
    echo "Rust CI workflow must ${description}: missing '${expected}'" >&2
    exit 1
  fi
}

require_exact_line() {
  local expected="$1"
  local description="$2"

  if ! grep -Fxq -- "$expected" "$workflow"; then
    echo "Rust CI workflow must ${description}: missing exact line '${expected}'" >&2
    exit 1
  fi
}

extract_job() {
  local job_name="$1"

  awk -v job_name="$job_name" '
    $0 == "  " job_name ":" { in_job = 1 }
    in_job && $0 ~ /^  [[:alnum:]_-]+:$/ && $0 != "  " job_name ":" { exit }
    in_job { print }
  ' "$workflow"
}

extract_step() {
  local job_block="$1"
  local step_name="$2"

  awk -v step_name="$step_name" '
    $0 == "      - name: " step_name { in_step = 1 }
    in_step && /^      - name: / && $0 != "      - name: " step_name { exit }
    in_step { print }
  ' <<< "$job_block"
}

require_block_line() {
  local block="$1"
  local expected="$2"
  local description="$3"

  if ! grep -Fxq -- "$expected" <<< "$block"; then
    echo "Rust CI workflow must ${description}: missing '${expected}' in its scoped block" >&2
    exit 1
  fi
}

require_text 'backend-crate-test:' 'define a per-crate test job'
require_text 'name: ${{ matrix.package }} / test' 'scope every crate test check to its package'
require_text 'fail-fast: false' 'allow every crate result to complete'

for package in fusillade fusillade-core fusillade-arsenal onwards; do
  require_text "- package: ${package}" "test ${package} in the matrix"
done

require_text 'cargo_args: --all-features' 'exercise Onwards optional Fusillade integration'

require_text 'runs-on: ${{ matrix.runner }}' 'run matrix entries independently'
require_text 'cargo llvm-cov --package "${{ matrix.package }}"' 'compile and test one package per runner'
require_text 'name: rust-coverage-${{ matrix.package }}' 'upload per-package coverage artifacts'
require_text 'backend-dwctl-test-shard:' 'define parallel dwctl test partitions'
require_text 'partition: [1, 2, 3, 4]' 'split dwctl tests into four partitions'
require_text 'name: dwctl / test (${{ matrix.partition }}/4)' 'name each dwctl partition independently'
require_text 'uses: taiki-e/install-action@nextest' 'install the nextest partition runner'
require_text 'source <(cargo llvm-cov show-env --export-prefix)' 'instrument nextest with cargo-llvm-cov'
require_text 'LLVM_PROFILE_FILE="$PWD/target/dwctl-${{ matrix.partition }}-%32m.profraw"' 'bound per-process coverage profiles with an LLVM merge pool'
require_text 'cargo nextest run --package dwctl' 'run each dwctl partition directly through nextest'
require_text '--cargo-profile ci' 'compile dwctl partitions with the lean Cargo profile'
require_text '--partition "count:${{ matrix.partition }}/4"' 'select one exhaustive dwctl count partition per runner'
require_text 'cargo llvm-cov report --profile ci' 'export each pooled partition as LCOV'
require_text 'name: rust-coverage-dwctl-${{ matrix.partition }}' 'upload each dwctl coverage partition independently'
require_text 'backend-dwctl-test:' 'preserve a dedicated aggregate dwctl test gate'
require_exact_line '    name: dwctl / test' 'preserve the required dwctl test context'
require_text 'name: workspace / rust lint' 'scope Rust linting to the workspace'
require_text 'needs: [changes, backend-crate-test, backend-dwctl-test, backend-lint, frontend-test, build]' \
  'gate backend-test on every crate, dwctl partition, lint, frontend test, and image build'
require_exact_line '    name: workspace / rust gate' 'name the aggregate Rust gate clearly'
require_text 'pattern: rust-coverage-*' 'download all per-package coverage artifacts'
require_text 'MINIMUM_COVERAGE: "60"' 'preserve the aggregate line coverage threshold'
require_text '.github/scripts/aggregate-rust-coverage.py' 'merge duplicate source lines before checking coverage'
require_text 'Expected 8 coverage files' 'aggregate every workspace crate coverage artifact'
require_text 'cargo package --locked --package onwards --all-features' 'validate the publishable Onwards package'
require_text 'name: onwards / image' 'scope the standalone image build to Onwards'
require_text 'name: dwctl / image' 'scope the control-layer image build to dwctl'
require_text 'check: dwctl / open responses' 'scope embedded compliance to dwctl'
require_text 'check: onwards / open responses (passthrough)' 'exercise standalone Onwards passthrough against the dwctl edge'
require_text 'name: ${{ matrix.check }}' 'name each compliance leg from its matrix entry'
require_text 'http://127.0.0.1:3001/ai/v1/' 'route Onwards passthrough compliance through the dwctl edge'
require_text 'name: dwctl / security' 'scope image scanning to dwctl'
require_text 'name: workspace / e2e' 'scope end-to-end validation to the workspace'
require_text 'GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}' 'reuse the control-layer Gemini compliance provider'
require_text 'https://generativelanguage.googleapis.com/v1beta/openai/' 'target the proven Gemini OpenAI-compatible endpoint'
require_text 'TEST_MODEL: gemini-2.5-flash' 'reuse the control-layer compliance model'
require_text 'OPENRESPONSES_COMPLIANCE_FILTER:' 'reuse the supported Open Responses compliance filter'
require_text 'git clone --depth 1 https://github.com/openresponses/openresponses /tmp/openresponses' 'track the current Open Responses compliance suite'

if grep -Fq 'workflow_dispatch' "$workflow"; then
  echo "Required-check CI must only run for pull requests and merge groups" >&2
  exit 1
fi

merge_group_trigger="$(sed -n '/^  merge_group:/,/^jobs:/p' "$workflow")"
require_block_line "$merge_group_trigger" '  merge_group:' 'listen for merge-group checks'
require_block_line "$merge_group_trigger" '    types: [checks_requested]' 'only run CI for requested merge-group checks'

onwards_image_job="$(extract_job onwards-pr-image)"
dwctl_image_job="$(extract_job build)"
crate_test_job="$(extract_job backend-crate-test)"

if grep -Fq -- '- package: dwctl' <<< "$crate_test_job"; then
  echo "dwctl must run in its partition matrix, not the generic crate matrix" >&2
  exit 1
fi

require_block_line "$crate_test_job" '    if: always()' 'expand the crate matrix for release-only changes'
require_block_line "$crate_test_job" "      RUN_CI: \${{ needs.changes.outputs.run-ci }}" 'expose the release-only decision to crate test steps'
crate_skip_step="$(extract_step "$crate_test_job" 'Skip crate tests for release-only changes')"
require_block_line "$crate_skip_step" "        if: env.RUN_CI != 'true'" 'declare the no-op crate test path'

if ! grep -Fxq '    needs: changes' <<< "$onwards_image_job" || \
   ! grep -Fxq '    needs: changes' <<< "$dwctl_image_job"; then
  echo "Onwards and dwctl image builds must start together after change classification" >&2
  exit 1
fi

trusted_pull_request_condition="github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name == github.repository && github.event.pull_request.user.login != 'dependabot[bot]'"

require_block_line "$onwards_image_job" "    if: needs.changes.outputs.run-ci == 'true' && ${trusted_pull_request_condition}" 'run Onwards image publishing only for relevant trusted pull requests'
require_block_line "$onwards_image_job" "          tags: ghcr.io/doublewordai/onwards:sha-\${{ github.event_name == 'pull_request' && github.event.pull_request.head.sha || github.sha }}" 'tag Onwards images with the PR head or merge-group SHA'
require_block_line "$dwctl_image_job" "    if: needs.changes.outputs.run-ci == 'true' && ${trusted_pull_request_condition}" 'run dwctl image publishing only for relevant trusted pull requests'
require_block_line "$dwctl_image_job" '          DOCKER_METADATA_PR_HEAD_SHA: true' 'preserve PR-head metadata tagging for dwctl images'
require_block_line "$dwctl_image_job" '            type=sha,prefix=sha-' 'preserve SHA metadata tagging for dwctl images'
if grep -Fq 'type=raw,value=sha-' <<< "$dwctl_image_job"; then
  echo "dwctl image tags must use docker metadata SHA handling, not a raw full SHA" >&2
  exit 1
fi
if grep -Fq "github.event_name == 'merge_group'" <<< "$onwards_image_job" || \
   grep -Fq "github.event_name == 'merge_group'" <<< "$dwctl_image_job" || \
   grep -Fq "github.actor != 'dependabot[bot]'" <<< "$onwards_image_job" || \
   grep -Fq "github.actor != 'dependabot[bot]'" <<< "$dwctl_image_job"; then
  echo "Image publishing must classify trust from pull-request provenance" >&2
  exit 1
fi

pr_title_workflow=".github/workflows/pr-title-check.yml"
pr_title_job="$(awk '/^  check-title:/{ in_job = 1 } in_job { print }' "$pr_title_workflow")"
semantic_title_step="$(extract_step "$pr_title_job" 'Validate pull request title')"
merge_group_title_step="$(extract_step "$pr_title_job" 'Skip pull request title validation for merge-group commits')"
if ! grep -Fq '  merge_group:' "$pr_title_workflow" || \
   ! grep -Fq '    types: [checks_requested]' "$pr_title_workflow"; then
  echo "PR title check must emit its required context for merge-group commits without reading PR data" >&2
  exit 1
fi
require_block_line "$semantic_title_step" "        if: github.event_name == 'pull_request'" 'limit the semantic title action to pull-request events'
require_block_line "$semantic_title_step" '        uses: amannn/action-semantic-pull-request@v6' 'run the semantic title action in its pull-request step'
require_block_line "$merge_group_title_step" "        if: github.event_name == 'merge_group'" 'limit the merge-group title no-op to merge-group events'
require_block_line "$merge_group_title_step" '        run: echo "Pull request title was validated before this merge-group commit was queued."' 'run the merge-group title no-op in its own step'

required_check_names=(
  'dashboard / test'
  'dwctl / test'
  'fusillade / test'
  'fusillade-core / test'
  'fusillade-arsenal / test'
  'onwards / test'
  'workspace / rust lint'
  'workspace / rust gate'
  'onwards / image'
  'dwctl / image'
  'dwctl / open responses'
  'onwards / open responses (passthrough)'
  'dwctl / security'
  'workspace / e2e'
  'workspace / pull request title'
)
actual_check_names=()
while IFS= read -r name; do
  case "$name" in
    '${{ matrix.package }} / test')
      actual_check_names+=(
        'fusillade / test'
        'fusillade-core / test'
        'fusillade-arsenal / test'
        'onwards / test'
      )
      ;;
    'dwctl / test (${{ matrix.partition }}/4)')
      # Partition checks are diagnostic fan-out jobs. The aggregate
      # `dwctl / test` context below remains the required branch-protection gate.
      ;;
    'release-only changes')
      # Change classification is an internal fan-out job, not a required
      # branch-protection context.
      ;;
    '${{ matrix.check }}')
      actual_check_names+=(
        'dwctl / open responses'
        'onwards / open responses (passthrough)'
      )
      ;;
    *) actual_check_names+=("$name") ;;
  esac
done < <(awk '/^    name: / { sub(/^    name: /, ""); print }' "$workflow" "$pr_title_workflow")

if ! diff -u \
  <(printf '%s\n' "${required_check_names[@]}") \
  <(printf '%s\n' "${actual_check_names[@]}"); then
  echo "CI and PR-title workflows must declare exactly the 15 repository-required check contexts" >&2
  exit 1
fi

setup_just_count="$(grep -Fc 'uses: extractions/setup-just@v3' "$workflow")"
pinned_just_count="$(grep -Fc 'just-version: "1.46.0"' "$workflow")"
if [[ "$setup_just_count" != "$pinned_just_count" ]]; then
  echo "Every setup-just invocation must pin just-version 1.46.0" >&2
  exit 1
fi
