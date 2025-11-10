#!/bin/bash
set -e

echo "=== Comprehensive Cargo Build Benchmark ==="
echo "This measures build times across different configurations"
echo ""

RESULTS="build_benchmark_$(date +%Y%m%d_%H%M%S).txt"

# Measure a single build
time_build() {
    local label="$1"
    local command="$2"
    
    echo "" | tee -a "$RESULTS"
    echo "=== $label ===" | tee -a "$RESULTS"
    
    # Warm up filesystem cache
    sync
    
    # Time the build
    START=$(date +%s)
    eval "$command" 2>&1 | tee /tmp/build_temp.log | grep -E "(Compiling|Finished)" | tail -3
    END=$(date +%s)
    DURATION=$((END - START))
    
    # Extract stats
    CRATES=$(grep -c "Compiling" /tmp/build_temp.log || echo "0")
    
    echo "Duration: ${DURATION}s" | tee -a "$RESULTS"
    echo "Crates compiled: $CRATES" | tee -a "$RESULTS"
    
    # Check for errors
    if grep -q "error:" /tmp/build_temp.log; then
        echo "⚠️  Build had errors" | tee -a "$RESULTS"
    fi
}

echo "Starting benchmarks at $(date)" > "$RESULTS"
echo "CPU info:" >> "$RESULTS"
nproc >> "$RESULTS"
echo "" >> "$RESULTS"

# Benchmark 1: Clean build without embedded-db
cargo clean
time_build "Clean Build (no embedded-db)" "cargo build --no-default-features"

# Benchmark 2: Incremental rebuild (no changes)
time_build "Incremental (no changes)" "cargo build --no-default-features"

# Benchmark 3: Incremental with small change
touch dwctl/src/main.rs
time_build "Incremental (touch main.rs)" "cargo build --no-default-features"

# Benchmark 4: Check-only (faster, no codegen)
cargo clean
time_build "cargo check --no-default-features" "cargo check --no-default-features"

# Benchmark 5: Release build
cargo clean
time_build "Release build (clean)" "cargo build --release --no-default-features"

# Summary statistics
echo "" | tee -a "$RESULTS"
echo "=== Dependency Analysis ===" | tee -a "$RESULTS"
echo "Total dependency tree size:" | tee -a "$RESULTS"
cargo tree --no-default-features -e normal | wc -l | tee -a "$RESULTS"

echo "" | tee -a "$RESULTS"
echo "Top-level dependencies:" | tee -a "$RESULTS"
cargo tree --no-default-features --depth 1 | grep -E "^[├└]" | wc -l | tee -a "$RESULTS"

echo "" | tee -a "$RESULTS"
echo "=== Results saved to: $RESULTS ==="
cat "$RESULTS"
