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
require_text 'needs: [backend-crate-test, backend-dwctl-test, backend-lint, frontend-test, build]' \
  'gate backend-test on every crate, dwctl partition, lint, frontend test, and image build'
require_exact_line '    name: workspace / rust gate' 'name the aggregate Rust gate clearly'
require_text 'pattern: rust-coverage-*' 'download all per-package coverage artifacts'
require_text 'MINIMUM_COVERAGE: "60"' 'preserve the aggregate line coverage threshold'
require_text '.github/scripts/aggregate-rust-coverage.py' 'merge duplicate source lines before checking coverage'
require_text 'Expected 8 coverage files' 'aggregate every workspace crate coverage artifact'
require_text 'cargo package --locked --package onwards --all-features' 'validate the publishable Onwards package'
require_text 'onwards-openresponses-compliance:' 'define standalone Onwards compliance'
require_text 'mode: [adapter, passthrough]' 'test Onwards adapter and passthrough modes'
require_text 'name: onwards / image' 'scope the standalone image build to Onwards'
require_text 'name: onwards / compliance changes' 'scope the compliance change detector to Onwards'
require_text 'name: onwards / open responses (${{ matrix.mode }})' 'scope standalone compliance checks to Onwards'
require_text 'name: dwctl / image' 'scope the control-layer image build to dwctl'
require_text 'name: dwctl / open responses' 'scope embedded compliance to dwctl'
require_text 'name: dwctl / security' 'scope image scanning to dwctl'
require_text 'name: workspace / e2e' 'scope end-to-end validation to the workspace'
require_text 'GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}' 'reuse the control-layer Gemini compliance provider'
require_text 'https://generativelanguage.googleapis.com/v1beta/openai/' 'target the proven Gemini OpenAI-compatible endpoint'
require_text 'TEST_MODEL: gemini-2.5-flash' 'reuse the control-layer compliance model'
require_text 'OPENRESPONSES_COMPLIANCE_FILTER:' 'reuse the supported Open Responses compliance filter'
require_text '--port 3001' 'run a local adapter upstream for passthrough compliance'
require_text 'http://127.0.0.1:3001/v1' 'route standalone passthrough compliance through the local adapter upstream'
require_text 'git clone --depth 1 https://github.com/openresponses/openresponses /tmp/openresponses' 'track the current Open Responses compliance suite'
require_text 'onwards-openresponses-${MODE}-retry.json' 'retry only transiently failing Open Responses filters'
require_text 'Malformed compliance output is always a failure.' 'reject malformed compliance runner output'

if grep -Fq 'workflow_dispatch' "$workflow"; then
  echo "Required-check CI must only run for pull requests and merge groups" >&2
  exit 1
fi

merge_group_trigger="$(sed -n '/^  merge_group:/,/^jobs:/p' "$workflow")"
require_block_line "$merge_group_trigger" '  merge_group:' 'listen for merge-group checks'
require_block_line "$merge_group_trigger" '    types: [checks_requested]' 'only run CI for requested merge-group checks'

onwards_compliance_job="$(extract_job onwards-openresponses-compliance)"
onwards_image_job="$(extract_job onwards-pr-image)"
dwctl_image_job="$(extract_job build)"
crate_test_job="$(extract_job backend-crate-test)"
dwctl_shard_job="$(extract_job backend-dwctl-test-shard)"

require_block_line "$dwctl_shard_job" '    env:' \
  'declare the compiler cache environment for every dwctl shard'
require_block_line "$dwctl_shard_job" '      CARGO_INCREMENTAL: "0"' \
  'disable incremental compilation so dwctl artifacts are cacheable'
require_block_line "$dwctl_shard_job" '      RUSTC_WRAPPER: sccache' \
  'compile dwctl through sccache'
require_block_line "$dwctl_shard_job" '      SCCACHE_IGNORE_SERVER_IO_ERROR: "1"' \
  'fall back to rustc when the remote cache is unavailable'
require_block_line "$dwctl_shard_job" '      - name: Install sccache' \
  'install sccache before compiling dwctl'
require_block_line "$dwctl_shard_job" \
  '        uses: mozilla-actions/sccache-action@v0.0.10' \
  'use the current sccache GitHub Action'

dwctl_sccache_step="$(extract_step "$dwctl_shard_job" 'Install sccache')"
require_block_line "$dwctl_sccache_step" '          version: "v0.16.0"' \
  'pin the sccache compiler cache version'

dwctl_rust_cache_step="$(
  extract_step "$dwctl_shard_job" 'Cache Rust dependencies and tools'
)"
require_block_line "$dwctl_rust_cache_step" '          cache-targets: "false"' \
  'avoid restoring target artifacts that cargo-llvm-cov immediately deletes'

if grep -Eq 'SCCACHE_(GHA_ENABLED|WEBDAV_(TOKEN|USERNAME|PASSWORD))' \
  <<< "$dwctl_shard_job"; then
  echo "Depot runners must provide sccache authentication without GitHub cache mode or a long-lived token" >&2
  exit 1
fi

if grep -Fq -- '- package: dwctl' <<< "$crate_test_job"; then
  echo "dwctl must run in its partition matrix, not the generic crate matrix" >&2
  exit 1
fi

if grep -Eq '^[[:space:]]+needs:' <<< "$onwards_image_job" || \
   grep -Eq '^[[:space:]]+needs:' <<< "$dwctl_image_job"; then
  echo "Onwards and dwctl image builds must start immediately instead of waiting for tests" >&2
  exit 1
fi

if grep -Fq 'OPENAI_API_KEY' <<< "$onwards_compliance_job"; then
  echo "Standalone Onwards compliance must reuse the existing Gemini provider" >&2
  exit 1
fi

if grep -Fq 'git checkout fa29df5' <<< "$onwards_compliance_job"; then
  echo "Standalone Onwards compliance must track the same current suite as control-layer compliance" >&2
  exit 1
fi

require_block_line "$onwards_compliance_job" "    if: always()" 'always expand the Onwards compliance matrix after the change detector'
require_block_line "$onwards_compliance_job" "    needs: onwards-compliance-changes" 'wait for the Onwards compliance change detector'
trusted_pull_request_condition="github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name == github.repository && github.event.pull_request.user.login != 'dependabot[bot]'"
require_block_line "$onwards_compliance_job" "      RUN_STRICT_COMPLIANCE: \${{ needs.onwards-compliance-changes.outputs.strict == 'true' && ${trusted_pull_request_condition} }}" 'only run strict compliance for trusted pull requests'
if grep -Fq "github.event_name == 'merge_group'" <<< "$onwards_compliance_job" || \
   grep -Fq "github.actor != 'dependabot[bot]'" <<< "$onwards_compliance_job"; then
  echo "Onwards compliance must classify trust from pull-request provenance" >&2
  exit 1
fi

skip_step="$(extract_step "$onwards_compliance_job" 'Skip strict compliance for untrusted or unchanged events')"
require_block_line "$skip_step" "        if: env.RUN_STRICT_COMPLIANCE != 'true'" 'declare the no-op strict compliance path'

for step_name in \
  'Checkout code' \
  'Require Gemini API key' \
  'Install Rust' \
  'Build Onwards' \
  'Install Bun' \
  'Clone Open Responses' \
  'Install Open Responses dependencies' \
  'Write Gemini adapter config' \
  'Start Gemini adapter upstream' \
  'Write Onwards config' \
  'Start Onwards' \
  'Run compliance tests'; do
  compliance_step="$(extract_step "$onwards_compliance_job" "$step_name")"
  require_block_line "$compliance_step" "        if: env.RUN_STRICT_COMPLIANCE == 'true'" "guard Onwards compliance step '${step_name}' with the trust gate"
done

for step_name in \
  'Show Onwards logs' \
  'Upload Onwards compliance artifacts' \
  'Stop Onwards processes'; do
  compliance_step="$(extract_step "$onwards_compliance_job" "$step_name")"
  require_block_line "$compliance_step" "        if: always() && env.RUN_STRICT_COMPLIANCE == 'true'" "guard Onwards compliance diagnostic step '${step_name}' with the trust gate"
done

require_block_line "$onwards_image_job" "    if: ${trusted_pull_request_condition}" 'run Onwards image publishing only for trusted pull requests'
require_block_line "$onwards_image_job" "          tags: ghcr.io/doublewordai/onwards:sha-\${{ github.event_name == 'pull_request' && github.event.pull_request.head.sha || github.sha }}" 'tag Onwards images with the PR head or merge-group SHA'
require_block_line "$dwctl_image_job" "    if: ${trusted_pull_request_condition}" 'run dwctl image publishing only for trusted pull requests'
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
  'onwards / compliance changes'
  'onwards / open responses (adapter)'
  'onwards / open responses (passthrough)'
  'dwctl / image'
  'dwctl / open responses'
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
    'onwards / open responses (${{ matrix.mode }})')
      actual_check_names+=(
        'onwards / open responses (adapter)'
        'onwards / open responses (passthrough)'
      )
      ;;
    *) actual_check_names+=("$name") ;;
  esac
done < <(awk '/^    name: / { sub(/^    name: /, ""); print }' "$workflow" "$pr_title_workflow")

if ! diff -u \
  <(printf '%s\n' "${required_check_names[@]}") \
  <(printf '%s\n' "${actual_check_names[@]}"); then
  echo "CI and PR-title workflows must declare exactly the 17 repository-required check contexts" >&2
  exit 1
fi

setup_just_count="$(grep -Fc 'uses: extractions/setup-just@v3' "$workflow")"
pinned_just_count="$(grep -Fc 'just-version: "1.46.0"' "$workflow")"
if [[ "$setup_just_count" != "$pinned_just_count" ]]; then
  echo "Every setup-just invocation must pin just-version 1.46.0" >&2
  exit 1
fi
