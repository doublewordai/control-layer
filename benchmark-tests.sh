#!/usr/bin/env bash
set -euo pipefail

echo "Stopping and removing old postgres..."
just db-stop --remove > /dev/null 2>&1 || true

echo "Starting fresh postgres..."
just db-start > /dev/null 2>&1

echo "Setting up databases..."
just db-setup > /dev/null 2>&1

echo "Compiling tests..."
cargo test --lib --no-run > /dev/null 2>&1

echo "Running test suite (with default parallelism: 4 threads)..."
start=$(date +%s.%N)
cargo test --lib > /dev/null 2>&1
end=$(date +%s.%N)

runtime=$(echo "$end - $start" | bc)
echo "Test suite completed in ${runtime}s"

echo "Cleaning up..."
just db-stop --remove > /dev/null 2>&1 || true
