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

for package in dwctl fusillade fusillade-core fusillade-arsenal; do
  require_text "- package: ${package}" "test ${package} in the matrix"
done

require_text 'runs-on: ${{ matrix.runner }}' 'run matrix entries independently'
require_text 'cargo llvm-cov --package "${{ matrix.package }}"' 'compile and test one package per runner'
require_text 'name: rust-coverage-${{ matrix.package }}' 'upload per-package coverage artifacts'
require_text 'backend-lint:' 'run workspace linting independently of crate tests'
require_text 'needs: [backend-crate-test, backend-lint]' 'gate backend-test on every crate and lint'
require_text 'name: backend-test' 'preserve the required backend check name'
require_text 'pattern: rust-coverage-*' 'download all per-package coverage artifacts'
require_text 'MINIMUM_COVERAGE: "60"' 'preserve the aggregate line coverage threshold'
require_text '.github/scripts/aggregate-rust-coverage.py' 'merge duplicate source lines before checking coverage'
