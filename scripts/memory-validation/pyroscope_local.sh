#!/usr/bin/env bash
#
# Local Pyroscope smoke test for dwctl heap profiles.
#
# Downloads a pinned Grafana Pyroscope release (linux/amd64) into a cache under
# $HOME/.cache, verifies it against the release's checksums.txt, starts a
# single-binary server with local filesystem storage in a temp directory,
# ingests a pprof file, queries it back over the Pyroscope Connect API, asserts
# that the requested function names are present, then stops the server.
#
# The binary and all server state stay outside the repository. The repository
# is only read for the default test profile generator.
#
# Usage:
#   pyroscope_local.sh [--profile FILE] [--assert-function NAME] [--port N] [--keep]
#
# With no --profile it generates a tiny pprof using the handwritten encoder in
# test_analyze_pprof.py, so the test needs no real jemalloc profile.
#
# Exit status is non-zero if the download/checksum fails, the server does not
# become ready, or the function is not found. If the download is impossible the
# script prints the exact curl/checksum error.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

VERSION="${PYROSCOPE_VERSION:-2.3.1}"
ARCH="linux_amd64"
CACHE_DIR="${PYROSCOPE_CACHE_DIR:-$HOME/.cache/dwctl-memory-validation/pyroscope}"
PORT="${PYROSCOPE_PORT:-14040}"
ASSERT_FUNCTION="${PYROSCOPE_ASSERT_FUNCTION:-retain_validation_ballast}"
PROFILE=""
KEEP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile) PROFILE="$2"; shift 2 ;;
    --assert-function) ASSERT_FUNCTION="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }

ASSET="pyroscope_${VERSION}_${ARCH}.tar.gz"
RELEASE_BASE="https://github.com/grafana/pyroscope/releases/download/v${VERSION}"

mkdir -p "$CACHE_DIR"
mkdir -p "$CACHE_DIR/$VERSION"

if [[ ! -x "$CACHE_DIR/$VERSION/pyroscope" ]]; then
  echo "==> downloading Pyroscope $VERSION ($ARCH)"
  if [[ ! -f "$CACHE_DIR/$ASSET" ]]; then
    if ! curl -fSL --retry 3 --retry-delay 2 -o "$CACHE_DIR/$ASSET" "$RELEASE_BASE/$ASSET"; then
      die "could not download $RELEASE_BASE/$ASSET (network or release missing)"
    fi
  fi
  if ! curl -fSL --retry 3 --retry-delay 2 -o "$CACHE_DIR/checksums.txt" "$RELEASE_BASE/checksums.txt"; then
    die "could not download $RELEASE_BASE/checksums.txt"
  fi
  expected="$(grep -E "[[:space:]]${ASSET}\$" "$CACHE_DIR/checksums.txt" | awk '{print $1}' | head -n1)"
  [[ -n "$expected" ]] || die "no checksum entry for $ASSET in checksums.txt"
  actual="$(sha256sum "$CACHE_DIR/$ASSET" | awk '{print $1}')"
  if [[ "$expected" != "$actual" ]]; then
    rm -f "$CACHE_DIR/$ASSET"
    die "checksum mismatch for $ASSET: expected $expected, got $actual"
  fi
  echo "==> checksum OK ($actual)"
  tar xzf "$CACHE_DIR/$ASSET" -C "$CACHE_DIR/$VERSION"
fi

PYROSCOPE="$CACHE_DIR/$VERSION/pyroscope"
[[ -x "$PYROSCOPE" ]] || die "pyroscope binary not found at $PYROSCOPE"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/dwctl-pyroscope-validation.XXXXXX")"
PYRO_PID=""
cleanup() {
  if [[ -n "$PYRO_PID" ]]; then
    kill "$PYRO_PID" 2>/dev/null || true
    wait "$PYRO_PID" 2>/dev/null || true
  fi
  if [[ "$KEEP" -eq 1 ]]; then
    echo "==> kept working dir: $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

if [[ -z "$PROFILE" ]]; then
  PROFILE="$WORK/test.pprof.gz"
  python3 "$SCRIPT_DIR/test_analyze_pprof.py" --emit-test-profile "$PROFILE" >/dev/null
  echo "==> generated test profile: $PROFILE"
fi
[[ -f "$PROFILE" ]] || die "profile not found: $PROFILE"

# Run from the temp dir so every relative default in the server config lands
# there rather than in the checkout. `-enable-query-backend-from` pins the read
# path to the v2 backend; "auto" reports no data for a fresh local server.
cd "$WORK"
echo "==> starting Pyroscope on 127.0.0.1:$PORT"
"$PYROSCOPE" \
  -server.http-listen-address=127.0.0.1 \
  -server.http-listen-port="$PORT" \
  -storage.backend=filesystem \
  -storage.filesystem.dir="$WORK/storage" \
  -metastore.data-dir="$WORK/metastore/data" \
  -metastore.raft.dir="$WORK/metastore/raft" \
  -metastore.raft.snapshots-dir="$WORK/metastore/raft" \
  -compactor.data-dir="$WORK/compactor" \
  -self-profiling.disable-push=true \
  -enable-query-backend-from=2020-01-01T00:00:00Z \
  -memberlist.bind-port="$((PORT + 2000))" \
  > "$WORK/server.log" 2>&1 &
PYRO_PID=$!

ready=0
for _ in $(seq 1 120); do
  code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/ready" 2>/dev/null || true)"
  if [[ "$code" == "200" ]]; then
    ready=1
    break
  fi
  if ! kill -0 "$PYRO_PID" 2>/dev/null; then
    echo "--- server log ---" >&2
    tail -40 "$WORK/server.log" >&2 || true
    die "Pyroscope exited before becoming ready"
  fi
  sleep 1
done
[[ "$ready" -eq 1 ]] || { tail -40 "$WORK/server.log" >&2 || true; die "Pyroscope did not become ready in 120s"; }

NAME="dwctl-validation-$(date +%s)"
NOW_MS="$(( $(date +%s) * 1000 ))"
FROM_MS="$(( NOW_MS - 3600000 ))"
UNTIL_MS="$(( NOW_MS + 600000 ))"

echo "==> ingesting $PROFILE as $NAME"
ingest_code="$(curl -s -o "$WORK/ingest.out" -w '%{http_code}' -X POST \
  "http://127.0.0.1:$PORT/ingest?name=${NAME}&format=pprof" \
  -H 'Content-Type: application/octet-stream' \
  --data-binary "@$PROFILE")"
[[ "$ingest_code" == "200" ]] || { cat "$WORK/ingest.out" >&2; die "ingest returned HTTP $ingest_code"; }

# Query the profile types, then the stack traces. The profile type ID comes
# from the server rather than being hardcoded: a profile without a period type
# gets an ID with empty period fields.
found=0
last_names=""
for attempt in $(seq 1 30); do
  curl -s -X POST "http://127.0.0.1:$PORT/querier.v1.QuerierService/ProfileTypes" \
    -H 'Content-Type: application/json' \
    -d "{\"start\":$FROM_MS,\"end\":$UNTIL_MS}" > "$WORK/profiletypes.json" 2>/dev/null || true

  profile_type_id="$(python3 - "$WORK/profiletypes.json" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception:
    print("")
    sys.exit(0)
types = data.get("profileTypes", []) or []
for entry in types:
    if entry.get("sampleType") == "inuse_space":
        print(entry.get("ID", ""))
        sys.exit(0)
for entry in types:
    if entry.get("ID"):
        print(entry["ID"])
        sys.exit(0)
print("")
PY
)"

  if [[ -n "$profile_type_id" ]]; then
    curl -s -X POST "http://127.0.0.1:$PORT/querier.v1.QuerierService/SelectMergeStacktraces" \
      -H 'Content-Type: application/json' \
      -d "{\"profileTypeID\":\"$profile_type_id\",\"labelSelector\":\"{service_name=\\\"$NAME\\\"}\",\"start\":$FROM_MS,\"end\":$UNTIL_MS,\"maxNodes\":1000}" \
      > "$WORK/stacks.json" 2>/dev/null || true

    found="$(python3 - "$WORK/stacks.json" "$ASSERT_FUNCTION" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception:
    print("0")
    sys.exit(0)
names = (data.get("flamegraph") or {}).get("names", []) or []
print("1" if any(sys.argv[2] in n for n in names) else "0")
PY
)"
    last_names="$(python3 - "$WORK/stacks.json" <<'PY'
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except Exception:
    print("")
    sys.exit(0)
names = (data.get("flamegraph") or {}).get("names", []) or []
print(", ".join(n for n in names[:20]))
PY
)"
    if [[ "$found" == "1" ]]; then
      echo "==> query OK: '$ASSERT_FUNCTION' present (profile type $profile_type_id)"
      echo "    functions: $last_names"
      break
    fi
  fi
  sleep 1
done

if [[ "$found" != "1" ]]; then
  echo "--- sampled functions: ${last_names:-<none>}" >&2
  echo "--- server log tail ---" >&2
  tail -20 "$WORK/server.log" >&2 || true
  die "function '$ASSERT_FUNCTION' not found in Pyroscope query results"
fi

echo "==> Pyroscope smoke test passed"
