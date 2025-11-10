#!/bin/bash
set -e

echo "=== Cargo Build Benchmark Script ==="
echo "This will measure clean and incremental build times"
echo ""

# Configuration
RUNS=3
BUILD_FLAGS="--no-default-features"

# Results file
RESULTS_FILE="build_benchmark_results.txt"
echo "Build Benchmark Results - $(date)" > "$RESULTS_FILE"
echo "=========================================" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Function to measure build time
measure_build() {
    local build_type=$1
    local flags=$2
    
    echo "--- $build_type ---" | tee -a "$RESULTS_FILE"
    
    for i in $(seq 1 $RUNS); do
        if [ "$build_type" == "Clean Build" ]; then
            cargo clean
        fi
        
        echo "Run $i/$RUNS..."
        /usr/bin/time -f "Time: %E (real) | %U (user) | %S (sys)" \
            cargo build $flags 2>&1 | tee -a /tmp/build_run_$i.log | tail -1 | tee -a "$RESULTS_FILE"
    done
    
    echo "" >> "$RESULTS_FILE"
}

# 1. Clean build baseline (no default features)
echo ""
echo "=== Testing Clean Builds (no embedded-db) ==="
measure_build "Clean Build" "$BUILD_FLAGS"

# 2. Incremental build (no changes)
echo ""
echo "=== Testing Incremental Builds (no changes) ==="
measure_build "Incremental Build (no changes)" "$BUILD_FLAGS"

# 3. Small change incremental build
echo ""
echo "=== Testing Incremental Build (small change) ==="
echo "Making small change to trigger rebuild..."
touch dwctl/src/main.rs
measure_build "Incremental Build (main.rs touch)" "$BUILD_FLAGS"

# Count crates
echo "" | tee -a "$RESULTS_FILE"
echo "--- Dependency Statistics ---" | tee -a "$RESULTS_FILE"
echo "Total crates compiled:" | tee -a "$RESULTS_FILE"
cargo build $BUILD_FLAGS 2>&1 | grep "Compiling" | wc -l | tee -a "$RESULTS_FILE"

echo "" | tee -a "$RESULTS_FILE"
echo "Dependency tree size:" | tee -a "$RESULTS_FILE"
cargo tree $BUILD_FLAGS -e normal | wc -l | tee -a "$RESULTS_FILE"

echo ""
echo "=== Benchmark Complete ==="
echo "Results saved to: $RESULTS_FILE"
cat "$RESULTS_FILE"
