# Build Performance Optimization Guide

## Baseline Metrics

**Current build time**: 88.4 seconds (1m 28s)
- Configuration: `cargo build --no-default-features`
- Crates compiled: 370
- Parallelization: 6.6x (good CPU utilization)

## Benchmark Workflow

### 1. Run Baseline
```bash
./quick_benchmark.sh baseline
```

### 2. Make Optimization Changes
Example: Reduce tokio features in Cargo.toml

### 3. Test Optimization
```bash
./quick_benchmark.sh optimized
```

### 4. Compare Results
```bash
./compare_builds.sh benchmark_baseline_*.txt benchmark_optimized_*.txt
```

## Proposed Optimizations

### Priority 1: High Impact, Low Risk

#### A. Reduce tokio features (Est. 5-10% faster)
**Current**: `tokio = { version = "1.0", features = ["full"] }`

**Change to**:
```toml
tokio = { version = "1.0", features = [
  "macros",
  "rt-multi-thread", 
  "net",
  "fs",
  "io-util",
  "signal",
  "sync",
  "time"
] }
```

**Rationale**: "full" includes unused features like process, test-util, parking_lot

**Files to modify**:
- dwctl/Cargo.toml:24
- fusillade/Cargo.toml:20

#### B. Fix axum version duplication (Est. 10-15% faster)
**Current**: fusillade uses axum 0.7.9 in dev-dependencies, dwctl uses 0.8.6

**Change**: Update fusillade/Cargo.toml dev-dependencies to use axum 0.8

**Impact**: Eliminates duplicate compilation of entire axum ecosystem

### Priority 2: Medium Impact, Medium Risk

#### C. Make OpenTelemetry optional (Est. 10-15% for dev builds)
Move opentelemetry deps behind a feature flag so dev builds can skip it.

#### D. Disable embedded-db by default
Remove from default features in dwctl/Cargo.toml:17

### Priority 3: Long-term Improvements

#### E. Workspace-level dependency management
Deduplicate common dependencies at workspace level

#### F. Consider compile-time trade-offs
- Evaluate if all sqlx queries need compile-time checking
- Consider runtime schema generation for utoipa in dev builds

## Testing Methodology

1. **Consistency**: Always use `--no-default-features` for fair comparison
2. **Clean builds**: Run `cargo clean` before each benchmark
3. **Multiple runs**: Take average of 3 runs for final numbers
4. **Document**: Record all changes and results in git commits

## Expected Results

| Optimization | Est. Time Saved | Est. Crates Reduced |
|-------------|----------------|---------------------|
| Reduce tokio features | 4-9s | 10-20 |
| Fix axum duplication | 9-13s | 30-50 |
| Disable embedded-db | 0s* | 0 |
| Optional telemetry | 9-13s | 40-60 |
| **Combined** | **22-35s** | **80-130** |

*embedded-db already disabled with --no-default-features

**Target**: Reduce 88s → 53-66s (25-40% improvement)
