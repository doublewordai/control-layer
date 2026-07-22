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

require_text 'backend-crate-test:' 'define a per-crate test job'
require_text 'name: backend-test (${{ matrix.package }})' 'give every crate test its own check name'
require_text 'fail-fast: false' 'allow every crate result to complete'

for package in dwctl fusillade fusillade-core fusillade-arsenal onwards; do
  require_text "- package: ${package}" "test ${package} in the matrix"
done

require_text 'cargo_args: --all-features' 'exercise Onwards optional Fusillade integration'

require_text 'runs-on: ${{ matrix.runner }}' 'run matrix entries independently'
require_text 'cargo llvm-cov --package "${{ matrix.package }}"' 'compile and test one package per runner'
require_text 'name: rust-coverage-${{ matrix.package }}' 'upload per-package coverage artifacts'
require_text 'backend-lint:' 'run workspace linting independently of crate tests'
require_text 'needs: [backend-crate-test, backend-lint]' 'gate backend-test on every crate and lint'
require_text 'name: backend-test' 'preserve the required backend check name'
require_text 'pattern: rust-coverage-*' 'download all per-package coverage artifacts'
require_text 'MINIMUM_COVERAGE: "60"' 'preserve the aggregate line coverage threshold'
require_text '.github/scripts/aggregate-rust-coverage.py' 'merge duplicate source lines before checking coverage'
require_text 'Expected 5 coverage files' 'aggregate every workspace crate coverage artifact'
require_text 'cargo package --locked --package onwards --all-features' 'validate the publishable Onwards package'
require_text 'onwards-openresponses-compliance:' 'define standalone Onwards compliance'
require_text 'mode: [adapter, passthrough]' 'test Onwards adapter and passthrough modes'
require_text 'GEMINI_API_KEY: ${{ secrets.GEMINI_API_KEY }}' 'reuse the control-layer Gemini compliance provider'
require_text 'https://generativelanguage.googleapis.com/v1beta/openai/' 'target the proven Gemini OpenAI-compatible endpoint'
require_text 'TEST_MODEL: gemini-2.5-flash' 'reuse the control-layer compliance model'
require_text 'OPENRESPONSES_COMPLIANCE_FILTER:' 'reuse the supported Open Responses compliance filter'
require_text '--port 3001' 'run a local adapter upstream for passthrough compliance'
require_text 'http://127.0.0.1:3001/v1' 'route standalone passthrough compliance through the local adapter upstream'
require_text 'git clone --depth 1 https://github.com/openresponses/openresponses /tmp/openresponses' 'track the current Open Responses compliance suite'
require_text 'onwards-openresponses-${MODE}-retry.json' 'retry only transiently failing Open Responses filters'
require_text 'Malformed compliance output is always a failure.' 'reject malformed compliance runner output'

onwards_compliance_job="$(
  sed -n '/^  onwards-openresponses-compliance:/,/^  build:/p' "$workflow"
)"

if grep -Fq 'OPENAI_API_KEY' <<< "$onwards_compliance_job"; then
  echo "Standalone Onwards compliance must reuse the existing Gemini provider" >&2
  exit 1
fi

if grep -Fq 'git checkout fa29df5' <<< "$onwards_compliance_job"; then
  echo "Standalone Onwards compliance must track the same current suite as control-layer compliance" >&2
  exit 1
fi

setup_just_count="$(grep -Fc 'uses: extractions/setup-just@v3' "$workflow")"
pinned_just_count="$(grep -Fc 'just-version: "1.46.0"' "$workflow")"
if [[ "$setup_just_count" != "$pinned_just_count" ]]; then
  echo "Every setup-just invocation must pin just-version 1.46.0" >&2
  exit 1
fi
