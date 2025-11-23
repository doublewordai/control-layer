#!/usr/bin/env bash
set -euo pipefail

# Script to measure test execution time
# Usage: ./test_timing.sh [test_filter]

TEST_FILTER="${1:-group}"

echo "=================================="
echo "Test Performance Measurement"
echo "=================================="
echo "Filter: $TEST_FILTER"
echo ""

# Step 1: Compile tests without running
echo "Step 1: Compiling tests (cargo test --no-run)..."
COMPILE_START=$(date +%s.%N)
cargo test --no-run
COMPILE_END=$(date +%s.%N)
COMPILE_TIME=$(echo "$COMPILE_END - $COMPILE_START" | bc)
echo "✓ Compilation time: ${COMPILE_TIME}s"
echo ""

# Count the tests
TEST_COUNT=$(cargo test "$TEST_FILTER" -- --list 2>&1 | grep "test$" | wc -l)
echo "Found $TEST_COUNT tests matching '$TEST_FILTER'"
echo ""

# Step 2: Run the tests with timing (parallel)
echo "Step 2: Running tests in parallel (cargo test $TEST_FILTER)..."
RUN_START=$(date +%s.%N)
cargo test "$TEST_FILTER" 2>&1 | grep -E "(test result:|running [0-9]+ test)"
RUN_END=$(date +%s.%N)
RUN_TIME=$(echo "$RUN_END - $RUN_START" | bc)
echo ""
echo "✓ Test execution time: ${RUN_TIME}s"
echo ""

# Calculate average time per test
AVG_TIME=$(echo "scale=3; $RUN_TIME / $TEST_COUNT" | bc)

# Summary
echo "=================================="
echo "Summary"
echo "=================================="
echo "Test filter:        $TEST_FILTER"
echo "Tests found:        $TEST_COUNT"
echo "Compilation time:   ${COMPILE_TIME}s"
echo "Execution time:     ${RUN_TIME}s"
echo "Avg per test:       ${AVG_TIME}s"
echo "=================================="
