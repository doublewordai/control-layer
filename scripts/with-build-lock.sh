#!/usr/bin/env bash
# Run a command under a machine-wide lock so heavy cargo jobs started from
# different worktrees run one after another instead of competing for CPU and
# memory. Usage: scripts/with-build-lock.sh <command> [args...]
#
#   DW_BUILD_LOCK_FILE     lock path (default /tmp/control-layer-build.lock)
#   DW_BUILD_LOCK_TIMEOUT  seconds to wait for the lock (default 3600)
#   DW_NO_BUILD_LOCK=1     run without the lock
#   DW_BUILD_LOCK_HELD     set for the wrapped command; nested calls run directly
set -euo pipefail

if (($# == 0)); then
    echo "usage: $0 <command> [args...]" >&2
    exit 2
fi

# A nested call (lint -> script, cargo watch -> test-rust.sh) would otherwise
# wait on the lock its own parent holds. CI runners are not shared.
if [[ -n "${DW_BUILD_LOCK_HELD:-}" || "${DW_NO_BUILD_LOCK:-}" == 1 || -n "${CI:-}" ]]; then
    export DW_BUILD_LOCK_HELD=1
    exec "$@"
fi
export DW_BUILD_LOCK_HELD=1

if ! command -v flock >/dev/null 2>&1; then
    echo "build lock: flock not found (macOS: brew install flock); running unlocked" >&2
    exec "$@"
fi

# Fixed path rather than TMPDIR, which can differ per session on macOS.
lock="${DW_BUILD_LOCK_FILE:-/tmp/control-layer-build.lock}"
timeout="${DW_BUILD_LOCK_TIMEOUT:-3600}"
conflict=75

# The probe only decides whether to explain the wait; the real acquisition is below.
if ! flock -n "$lock" true; then
    echo "build lock: waiting for $lock (another heavy cargo job is running; DW_NO_BUILD_LOCK=1 skips)" >&2
fi

# -o: the command does not inherit the lock, so a stray child cannot keep it.
status=0
flock -o -w "$timeout" -E "$conflict" "$lock" "$@" || status=$?
if ((status == conflict)); then
    echo "build lock: gave up after ${timeout}s waiting for $lock (or the command exited $conflict)" >&2
fi
exit "$status"
