#!/usr/bin/env bash
#
# Build and run the dwctl memory-observability validation harness.
#
# Scenarios:
#   A. Phase validation with jemalloc sampling off (baseline/burst/release/quiet).
#   B. Phase validation with jemalloc sampling on
#      (_RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_sample:19).
#   C. Overhead workload, sampling off, 3 runs.
#   D. Overhead workload, sampling on, 3 runs.
#
# Everything lands in $OUT_DIR (default: target/memory-validation/<UTC stamp>).
# The script never edits the repository; profiles and JSON summaries are output.
#
# Environment overrides:
#   OUT_DIR      output directory
#   QUIET_SECS   quiet-phase seconds (default 30)
#   THREADS      worker threads for burst/churn (default 4)
#   ITERATIONS   churn iterations per thread in overhead mode (default 50000)
#   CARGO        cargo binary (default ~/.cargo/bin/cargo)
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
CARGO="$(command -v "$CARGO" || true)"
if [[ -z "$CARGO" ]]; then
  echo "error: cargo not found at $CARGO (set CARGO=...)" >&2
  exit 1
fi

OUT_DIR="${OUT_DIR:-$REPO_ROOT/target/memory-validation/$(date -u +%Y%m%dT%H%M%SZ)}"
QUIET_SECS="${QUIET_SECS:-30}"
THREADS="${THREADS:-4}"
ITERATIONS="${ITERATIONS:-50000}"
PROF_CONF="prof:true,prof_active:true,lg_prof_sample:19"

mkdir -p "$OUT_DIR"
echo "==> output: $OUT_DIR"

echo "==> building example (SQLX_OFFLINE=true)"
export PATH="$(dirname "$CARGO"):$PATH"
export SQLX_OFFLINE=true
(cd "$REPO_ROOT" && "$CARGO" build --release -p dwctl --no-default-features --example memory_validation)

BIN="$REPO_ROOT/target/release/examples/memory_validation"
if [[ ! -x "$BIN" ]]; then
  echo "error: expected binary at $BIN" >&2
  exit 1
fi

run_phase() {
  local label="$1"
  local prof_conf="$2"
  local dir="$OUT_DIR/$label"
  mkdir -p "$dir"
  echo "==> phase scenario $label (profiling: ${prof_conf:-off})"
  if [[ -n "$prof_conf" ]]; then
    _RJEM_MALLOC_CONF="$prof_conf" "$BIN" \
      --out "$dir" --quiet-secs "$QUIET_SECS" --threads "$THREADS" \
      > "$OUT_DIR/$label.json"
  else
    env -u _RJEM_MALLOC_CONF "$BIN" \
      --out "$dir" --quiet-secs "$QUIET_SECS" --threads "$THREADS" \
      > "$OUT_DIR/$label.json"
  fi
}

run_overhead() {
  local label="$1"
  local prof_conf="$2"
  local run="$3"
  local dir="$OUT_DIR/$label"
  mkdir -p "$dir"
  echo "==> overhead $label run $run (profiling: ${prof_conf:-off})"
  if [[ -n "$prof_conf" ]]; then
    _RJEM_MALLOC_CONF="$prof_conf" "$BIN" \
      --overhead --out "$dir" --threads "$THREADS" --iterations "$ITERATIONS" \
      > "$OUT_DIR/$label-$run.json"
  else
    env -u _RJEM_MALLOC_CONF "$BIN" \
      --overhead --out "$dir" --threads "$THREADS" --iterations "$ITERATIONS" \
      > "$OUT_DIR/$label-$run.json"
  fi
}

run_phase "scenario-a-off" ""
run_phase "scenario-b-on" "$PROF_CONF"

for run in 1 2 3; do
  run_overhead "overhead-off" "" "$run"
done
for run in 1 2 3; do
  run_overhead "overhead-on" "$PROF_CONF" "$run"
done

# Analyze scenario B's heap profiles, then require the quiet-phase profile to
# attribute most of the retained ballast (256 MiB) to its allocating function.
profile_count=0
for profile in "$OUT_DIR"/scenario-b-on/heap-*.pb.gz; do
  [[ -e "$profile" ]] || continue
  profile_count=$((profile_count + 1))
  echo "==> analyze $profile"
  python3 "$SCRIPT_DIR/analyze_pprof.py" "$profile" --top 15 || true
done
quiet_profile="$OUT_DIR/scenario-b-on/heap-quiet.pb.gz"
if [[ ! -e "$quiet_profile" ]]; then
  echo "error: sampling was on but no quiet-phase heap profile was produced" >&2
  exit 1
fi
echo "==> assert retained stack in $quiet_profile"
python3 "$SCRIPT_DIR/analyze_pprof.py" "$quiet_profile" --top 0 \
  --assert-function retain_validation_ballast --min-bytes $((200 * 1024 * 1024))

# Aggregate the overhead runs so on/off can be compared at a glance.
python3 - "$OUT_DIR" <<'PY'
import json
import glob
import os
import statistics
import sys

out_dir = sys.argv[1]

def load(pattern):
    rows = []
    for path in sorted(glob.glob(os.path.join(out_dir, pattern))):
        with open(path) as handle:
            rows.append(json.load(handle))
    return rows

def summarize(label, rows):
    if not rows:
        print(f"{label}: no runs")
        return
    wall = statistics.median(r["wall_ms"] for r in rows)
    cpu = statistics.median(r["user_sys_ms"] for r in rows)
    peak = [r.get("peak_rss_bytes") for r in rows if r.get("peak_rss_bytes")]
    peak_txt = f"{statistics.median(peak)/1024/1024:.1f} MiB" if peak else "n/a"
    print(f"{label}: median wall {wall} ms, median user+sys {cpu} ms, median peak RSS {peak_txt}")

off = load("overhead-off-*.json")
on = load("overhead-on-*.json")
summarize("overhead off", off)
summarize("overhead on ", on)
if off and on:
    off_wall = statistics.median(r["wall_ms"] for r in off)
    on_wall = statistics.median(r["wall_ms"] for r in on)
    off_cpu = statistics.median(r["user_sys_ms"] for r in off)
    on_cpu = statistics.median(r["user_sys_ms"] for r in on)
    if off_wall:
        print(f"wall-time overhead: {100.0 * (on_wall - off_wall) / off_wall:+.1f}%")
    if off_cpu:
        # CPU time is steadier than wall time when cores are contended; prefer
        # it when the two disagree.
        print(f"CPU overhead (user+sys): {100.0 * (on_cpu - off_cpu) / off_cpu:+.1f}%")
PY

echo "==> done. Summaries:"
echo "    $OUT_DIR/scenario-a-off.json"
echo "    $OUT_DIR/scenario-b-on.json"
echo "    $OUT_DIR/overhead-off-*.json"
echo "    $OUT_DIR/overhead-on-*.json"
