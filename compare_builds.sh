#!/bin/bash
# Compare two build configurations

if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <baseline_result.txt> <optimized_result.txt>"
    exit 1
fi

BASELINE=$1
OPTIMIZED=$2

echo "=== Build Performance Comparison ==="
echo ""

extract_metric() {
    local file=$1
    local pattern=$2
    grep "$pattern" "$file" | grep -oE '[0-9]+' | head -1
}

baseline_time=$(extract_metric "$BASELINE" "Duration:")
optimized_time=$(extract_metric "$OPTIMIZED" "Duration:")

baseline_crates=$(extract_metric "$BASELINE" "Crates compiled:")
optimized_crates=$(extract_metric "$OPTIMIZED" "Crates compiled:")

if [ -n "$baseline_time" ] && [ -n "$optimized_time" ]; then
    improvement=$(echo "scale=2; (($baseline_time - $optimized_time) / $baseline_time) * 100" | bc)
    echo "Build Time:"
    echo "  Baseline:  ${baseline_time}s"
    echo "  Optimized: ${optimized_time}s"
    echo "  Change:    ${improvement}%"
    echo ""
fi

if [ -n "$baseline_crates" ] && [ -n "$optimized_crates" ]; then
    reduction=$(echo "scale=2; (($baseline_crates - $optimized_crates) / $baseline_crates) * 100" | bc)
    echo "Crates Compiled:"
    echo "  Baseline:  $baseline_crates"
    echo "  Optimized: $optimized_crates"
    echo "  Reduction: ${reduction}%"
fi
