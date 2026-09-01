#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
aggregator="$repo_root/.github/scripts/aggregate-rust-coverage.py"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

cat > "$fixture/dwctl.info" <<'LCOV'
TN:
SF:/workspace/shared.rs
DA:1,1
DA:2,0
LF:2
LH:1
end_of_record
LCOV

cat > "$fixture/fusillade.info" <<'LCOV'
TN:
SF:/workspace/shared.rs
DA:2,3
DA:3,0
LF:2
LH:1
end_of_record
LCOV

output="$($aggregator --minimum 60 "$fixture/dwctl.info" "$fixture/fusillade.info")"
grep -Fxq 'lines_found=3' <<< "$output"
grep -Fxq 'lines_hit=2' <<< "$output"
grep -Fxq 'coverage=66.67' <<< "$output"

if "$aggregator" --minimum 70 "$fixture/dwctl.info" "$fixture/fusillade.info" \
  > "$fixture/below-threshold.out" 2> "$fixture/below-threshold.err"; then
  echo "Coverage below the threshold must fail" >&2
  exit 1
fi
grep -Fxq 'Backend coverage 66.67% is below 70.00%' "$fixture/below-threshold.err"

cat > "$fixture/empty.info" <<'LCOV'
TN:
SF:/workspace/empty.rs
LF:0
LH:0
end_of_record
LCOV

if "$aggregator" --minimum 0 "$fixture/empty.info" \
  > "$fixture/empty.out" 2> "$fixture/empty.err"; then
  echo "Coverage input without instrumented lines must fail" >&2
  exit 1
fi
grep -Fxq 'No instrumented Rust lines found' "$fixture/empty.err"
