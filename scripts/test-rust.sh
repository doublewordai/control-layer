#!/usr/bin/env bash
# Keep normal, watch, and coverage runs on the same runner and package selection.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
original_args=("$@")

watch=false
coverage=false
no_run=false
doctests=true
args=()
doc_args=()
default_workspace=true
default_all_features=true
while (($#)); do
    case "$1" in
        -p|--package|-p?*|--package=*|--workspace) default_workspace=false ;;
        -F|--features|-F?*|--features=*|--no-default-features|--all-features) default_all_features=false ;;
    esac
    case "$1" in
        --watch) watch=true; shift ;;
        --coverage) coverage=true; shift ;;
        --no-doctests) doctests=false; shift ;;
        --no-run) no_run=true; args+=("$1"); shift ;;
        -p|--package|--exclude|-F|--features|--target)
            if (($# < 2)); then echo "$1 requires a value" >&2; exit 2; fi
            args+=("$1" "$2"); doc_args+=("$1" "$2"); shift 2 ;;
        --cargo-profile)
            if (($# < 2)); then echo "$1 requires a value" >&2; exit 2; fi
            args+=("$1" "$2"); doc_args+=(--profile "$2"); shift 2 ;;
        --release|--locked|--offline|--no-default-features|--all-features|--workspace|-p?*|-F?*|--package=*|--exclude=*|--features=*|--target=*)
            args+=("$1"); doc_args+=("$1"); shift ;;
        --cargo-profile=*)
            args+=("$1"); doc_args+=("--profile=${1#*=}"); shift ;;
        --) args+=("$@"); break ;;
        *) args+=("$1"); shift ;;
    esac
done

# One heavy job per machine: hold the build lock across the test run and the
# doctest pass. Watch mode locks each inner run, not the idle watcher.
if ! $watch && [[ -z "${DW_BUILD_LOCK_HELD:-}" ]]; then
    exec ./scripts/with-build-lock.sh ./scripts/test-rust.sh ${original_args[@]+"${original_args[@]}"}
fi

if ! cargo nextest --version >/dev/null 2>&1; then
    echo "cargo-nextest is required: cargo install cargo-nextest --locked" >&2
    exit 1
fi

if $watch; then
    if ! command -v cargo-watch >/dev/null 2>&1; then
        echo "cargo-watch is required: cargo install cargo-watch --locked" >&2
        exit 1
    fi
    command=(./scripts/test-rust.sh)
    if $coverage; then command+=(--coverage); fi
    if ! $doctests; then command+=(--no-doctests); fi
    command+=(${args[@]+"${args[@]}"})
    printf -v watch_command '%q ' "${command[@]}"
    exec cargo watch -s "$watch_command"
fi

# Compile against the committed .sqlx metadata so the build needs no database.
# Tests still read DATABASE_URL when they run.
offline_defaulted=false
if [[ -z "${SQLX_OFFLINE+set}" ]]; then
    export SQLX_OFFLINE=true
    offline_defaulted=true
fi

defaults=()
if $default_workspace; then defaults+=(--workspace); fi
if $default_all_features; then defaults+=(--all-features); fi

status=0
if $coverage; then
    cargo llvm-cov nextest ${defaults[@]+"${defaults[@]}"} \
        --fail-under-lines 60 --lcov --output-path lcov.info ${args[@]+"${args[@]}"} || status=$?
else
    cargo nextest run ${defaults[@]+"${defaults[@]}"} ${args[@]+"${args[@]}"} || status=$?
fi

# 101 is nextest's exit code for a failed build.
if $offline_defaulted && ((status == 101)); then
    echo "test rust: the build ran with SQLX_OFFLINE=true. If it reports 'no cached data for this query'," >&2
    echo "  run ./scripts/sqlx-prepare.sh against a migrated database, or set SQLX_OFFLINE=false." >&2
fi

# Nextest doesn't execute doctests. Keep these even if an integration test fails.
# Runner filters (e.g. -E, -j, --partition) do not apply to rustdoc.
# --no-doctests skips this pass for the inner loop.
if ! $no_run && $doctests; then
    cargo test --doc ${defaults[@]+"${defaults[@]}"} ${doc_args[@]+"${doc_args[@]}"} || status=$?
fi
exit "$status"
