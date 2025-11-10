#!/bin/bash
# Quick benchmark script - measures clean build time

set -e

NAME=${1:-"current"}
RESULT_FILE="benchmark_${NAME}_$(date +%Y%m%d_%H%M%S).txt"

echo "=== Quick Build Benchmark: $NAME ===" | tee "$RESULT_FILE"
echo "Started: $(date)" | tee -a "$RESULT_FILE"
echo "" | tee -a "$RESULT_FILE"

# Clean and time build
cargo clean > /dev/null 2>&1

echo "Building with: cargo build --no-default-features" | tee -a "$RESULT_FILE"
echo "" | tee -a "$RESULT_FILE"

/usr/bin/time -f "Real: %E\nUser: %U\nSys: %S" \
  cargo build --no-default-features 2>&1 | tee -a "$RESULT_FILE"

# Extract crate count
CRATES=$(grep -c "Compiling" "$RESULT_FILE" || echo "unknown")
echo "" | tee -a "$RESULT_FILE"
echo "Crates compiled: $CRATES" | tee -a "$RESULT_FILE"

echo "" | tee -a "$RESULT_FILE"
echo "=== Results saved to: $RESULT_FILE ===" | tee -a "$RESULT_FILE"
