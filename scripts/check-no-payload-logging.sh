#!/usr/bin/env bash
#
# ZDR no-payload-logging guard (COR-500, part of COR-479).
#
# Fails if a Rust log/trace statement looks like it emits user prompt or model
# response payload content. Production runs at RUST_LOG=debug with OTLP export
# on, so any debug!/info!/warn!/error! field that carries a body, completion,
# streamed chunk, or provider error body is live-exported to Loki/Tempo.
#
# This is a heuristic line scanner, not a Rust parser. It targets the exact
# leak shapes the COR-479 cleanups removed so they cannot be reintroduced:
#   1. Debugging "sample" fields that snapshot a body/chunk.
#   2. Logging raw bytes via `= ?String::from_utf8_lossy(..)` /  `= %..lossy`.
#   3. Interpolating a whole payload-bearing variable as a tracing field, e.g.
#      `body = ?x`, `%response`, `?messages` (length fields like `body_len`,
#      `response_len`, `data_len` use a word boundary and are NOT flagged).
#
# Escape hatch: append `// zdr-allow: <reason>` to a line that is a verified
# false positive (e.g. a field genuinely carrying metadata, or test scaffolding
# that never runs in production).
#
# Usage: scripts/check-no-payload-logging.sh [DIR ...]   (default: dwctl/src)

set -euo pipefail

DIRS=("$@")
if [ ${#DIRS[@]} -eq 0 ]; then
    DIRS=("dwctl/src")
fi

# Prefer ripgrep; fall back to grep -rEn.
if command -v rg >/dev/null 2>&1; then
    search() { rg --no-heading --line-number --color never -e "$1" "${DIRS[@]}"; }
else
    search() { grep -rEn -- "$1" "${DIRS[@]}"; }
fi

# A payload-bearing word. Word boundaries keep metadata fields such as
# `body_len`, `response_size`, `content_type`, `request_id` safe.
WORD='(body|response|response_body|request_body|messages|prompt|completion|chunk|delta|payload|content)'

# (1) debugging body/chunk snapshot fields.
PAT_SAMPLE='\b(body_sample|data_sample|response_preview|prompt_sample|completion_sample|raw_body|response_body_sample)\b'
# (2) raw bytes logged as a tracing field value.
PAT_LOSSY='=[[:space:]]*[%?][[:space:]]*String::from_utf8_lossy'
# (3) a payload-bearing word used as a tracing field NAME, rendered via a
#     Debug/Display sigil:  `body = ?x`,  `response = %r`,  `messages = ?m`.
PAT_FIELD="\b${WORD}\b[[:space:]]*=[[:space:]]*[%?]"
# (4) a payload-bearing variable used as a tracing field VALUE:  `foo = ?body`.
PAT_VALUE="=[[:space:]]*[%?]${WORD}\b"

violations=""
for pat in "$PAT_SAMPLE" "$PAT_LOSSY" "$PAT_FIELD" "$PAT_VALUE"; do
    # Collect matches, drop allow-listed lines.
    hits="$(search "$pat" 2>/dev/null | grep -v 'zdr-allow' || true)"
    if [ -n "$hits" ]; then
        violations+="$hits"$'\n'
    fi
done

if [ -n "${violations//[$'\n']/}" ]; then
    echo "❌ ZDR no-payload-logging guard: potential prompt/response payload in a log statement." >&2
    echo "   Replace the payload with length/status metadata, or annotate a verified" >&2
    echo "   false positive with '// zdr-allow: <reason>'. See COR-479." >&2
    echo >&2
    printf '%s\n' "$violations" | sed '/^$/d' | sort -u >&2
    exit 1
fi

echo "✅ ZDR no-payload-logging guard: clean (${DIRS[*]})"
