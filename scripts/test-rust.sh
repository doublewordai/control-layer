#!/usr/bin/env bash
# Keep normal, watch, and coverage runs on the same runner and package selection.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

watch=false
coverage=false
no_run=false
args=()
doc_args=(--workspace --all-features)
while (($#)); do
    case "$1" in
        --watch) watch=true; shift ;;
        --coverage) coverage=true; shift ;;
        --no-run) no_run=true; args+=("$1"); shift ;;
        -p|--package|--exclude|--features|--target)
            if (($# < 2)); then echo "$1 requires a value" >&2; exit 2; fi
            args+=("$1" "$2"); doc_args+=("$1" "$2"); shift 2 ;;
        --cargo-profile)
            if (($# < 2)); then echo "$1 requires a value" >&2; exit 2; fi
            args+=("$1" "$2"); doc_args+=(--profile "$2"); shift 2 ;;
        --release|--locked|--offline|--no-default-features|--all-features|--workspace|--package=*|--exclude=*|--features=*|--target=*)
            args+=("$1"); doc_args+=("$1"); shift ;;
        --cargo-profile=*)
            args+=("$1"); doc_args+=("--profile=${1#*=}"); shift ;;
        --) args+=("$@"); break ;;
        *) args+=("$1"); shift ;;
    esac
done

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
    command+=(${args[@]+"${args[@]}"})
    printf -v watch_command '%q ' "${command[@]}"
    exec cargo watch -s "$watch_command"
fi

status=0
if $coverage; then
    cargo llvm-cov nextest --workspace --all-features \
        --fail-under-lines 60 --lcov --output-path lcov.info ${args[@]+"${args[@]}"} || status=$?
else
    cargo nextest run --workspace --all-features ${args[@]+"${args[@]}"} || status=$?
fi

# Nextest doesn't execute doctests. Keep these even if an integration test fails.
# Runner filters (e.g. -E, -j, --partition) do not apply to rustdoc.
if ! $no_run; then
    cargo test --doc "${doc_args[@]}" || status=$?
fi
exit "$status"
