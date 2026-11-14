#!/usr/bin/env bash
# Refresh, or with --check verify, the committed SQLx query metadata (.sqlx)
# for every target and feature, tests included. That scope is what lets the
# workspace compile with SQLX_OFFLINE=true and no database.
#
# Usage: scripts/sqlx-prepare.sh [--check]
#
# Needs DATABASE_URL (environment or .env) for a migrated database.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# sqlx's own --workspace only decides what to recompile; the cargo arguments
# after -- decide what is checked, and default to the default member (dwctl).
scope=(--workspace --all-targets --all-features)
check=false
case "${1:-}" in
    "") ;;
    --check) check=true ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

# `cargo sqlx prepare` verifies and records every query compiled while it runs.
# A dependency that ships its own .sqlx (underway) and is compiled in that
# window is verified against our database, which may not have its schema, and
# has its queries recorded as ours. Dependencies are only compiled when their
# build is not cached, so compile them first, offline, where nothing is
# recorded. Workspace crates with changed queries fail here, which is expected:
# --keep-going still builds every dependency.
echo "sqlx prepare: compiling dependencies..." >&2
precompile_log="$(mktemp)"
trap 'rm -f "$precompile_log"' EXIT
SQLX_OFFLINE=true cargo check "${scope[@]}" --keep-going >"$precompile_log" 2>&1 || true
# Shown only if the prepare step fails: a dependency that did not build above
# is compiled online below, and its errors then explain the failure.
show_precompile() {
    echo "sqlx prepare: output of the offline dependency build:" >&2
    tail -n 40 "$precompile_log" >&2
}

if $check; then
    if cargo sqlx prepare --check --workspace -- "${scope[@]}"; then exit 0; fi
    show_precompile
    echo "sqlx prepare: .sqlx is out of date; run ./scripts/sqlx-prepare.sh against a migrated database and commit .sqlx" >&2
    exit 1
fi
cargo sqlx prepare --workspace -- "${scope[@]}" || { status=$?; show_precompile; exit "$status"; }
