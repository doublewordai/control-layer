#!/usr/bin/env bash
# Guard the CI settings that keep Rust and container builds incremental.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

failures=0

require_literal() {
    local file=$1
    local literal=$2
    local description=$3

    if ! grep -Fq -- "$literal" "$file"; then
        printf 'FAIL: %s\n' "$description" >&2
        failures=$((failures + 1))
    fi
}

require_absent() {
    local file=$1
    local literal=$2
    local description=$3

    if grep -Fq -- "$literal" "$file"; then
        printf 'FAIL: %s\n' "$description" >&2
        failures=$((failures + 1))
    fi
}

require_profile_setting() {
    local setting=$1
    local profile

    profile=$(awk '
        /^\[profile\.ci\]$/ { found=1; next }
        /^\[/ { if (found) exit }
        found { print }
    ' Cargo.toml)

    if ! grep -Eq "^${setting}$" <<<"$profile"; then
        printf 'FAIL: profile.ci must contain %s\n' "$setting" >&2
        failures=$((failures + 1))
    fi
}

require_profile_setting 'inherits[[:space:]]*=[[:space:]]*"test"'
require_profile_setting 'debug[[:space:]]*=[[:space:]]*0'
require_profile_setting 'incremental[[:space:]]*=[[:space:]]*false'

require_literal justfile \
    'cargo llvm-cov --profile ci --no-clean --fail-under-lines 60' \
    'coverage builds must use the reusable lean CI profile'
require_absent justfile \
    'cargo llvm-cov show-env' \
    'coverage instrumentation must not leak into lint builds'

require_literal .github/workflows/ci.yaml \
    'uses: dtolnay/rust-toolchain@1.93.0' \
    'the backend CI Rust toolchain must be pinned'
require_literal .github/workflows/ci.yaml \
    'components: rustfmt, clippy, llvm-tools-preview' \
    'the pinned backend toolchain must include lint and coverage components'
require_literal .github/workflows/ci.yaml \
    'cache-workspace-crates: true' \
    'the backend cache must preserve the expensive workspace test crate'
require_literal .github/workflows/ci.yaml \
    'cache-on-failure: true' \
    'successful compilation artifacts must survive later test failures'
require_literal .github/workflows/ci.yaml \
    "cargo install sqlx-cli --version '0.8.6'" \
    'sqlx-cli must be pinned to the workspace SQLx version'
require_literal .github/workflows/ci.yaml \
    "cargo install cargo-llvm-cov --version '0.6.21'" \
    'cargo-llvm-cov must be pinned for stable caches'
require_literal .github/workflows/ci.yaml \
    'RUST_TEST_THREADS: 8' \
    'database tests must use the measured concurrency limit'
require_absent .github/workflows/ci.yaml \
    'working-directory: ./dwctl' \
    'backend coverage must be read from the repository root'

for postgres_setting in \
    '-c fsync=off' \
    '-c full_page_writes=off' \
    '-c synchronous_commit=off' \
    '-c wal_level=minimal' \
    '-c max_wal_senders=0' \
    '-c checkpoint_timeout=1h' \
    '-c max_wal_size=4GB' \
    '-c shared_buffers=256MB' \
    '-c work_mem=16MB' \
    '-c maintenance_work_mem=128MB'; do
    require_literal .github/workflows/ci.yaml "$postgres_setting" \
        "test Postgres must include ${postgres_setting}"
done

platforms_line="platforms: \${{ github.event_name == 'workflow_dispatch' && 'linux/amd64,linux/arm64' || 'linux/amd64' }}"
require_literal .github/workflows/ci.yaml "$platforms_line" \
    'automatic PR images must be amd64 while manual images stay multiarch'

require_literal Dockerfile \
    "id=dwctl-cargo-target-\${TARGETARCH}" \
    'Docker Cargo target caches must be architecture-specific'
require_literal Dockerfile \
    'target=/usr/local/cargo/registry' \
    'Docker builds must preserve the Cargo registry cache'
require_literal Dockerfile \
    'target=/usr/local/cargo/git' \
    'Docker builds must preserve the Cargo git cache'
require_literal Dockerfile \
    'cp target/release/dwctl /app/dwctl-bin' \
    'the cached release binary must be preserved outside the mount'
require_literal Dockerfile \
    'COPY --from=builder /app/dwctl-bin /app/dwctl' \
    'the runtime image must copy the preserved binary'

if ((failures > 0)); then
    printf 'CI performance guard found %d problem(s).\n' "$failures" >&2
    exit 1
fi

printf 'CI performance guard passed.\n'
