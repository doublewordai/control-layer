# Build Optimization Results

## Phase 1: Tokio Features + Axum Version

### Changes Made
1. **Reduced tokio features** from "full" to specific features
   - dwctl/Cargo.toml:24
   - fusillade/Cargo.toml:20
   - Changed from: `features = ["full"]`
   - Changed to: `features = ["macros", "rt-multi-thread", "net", "fs", "io-util", "signal", "sync", "time"]`

2. **Updated axum version** in fusillade dev-dependencies
   - fusillade/Cargo.toml:37
   - Changed from: `axum = "0.7"`
   - Changed to: `axum = "0.8"`

### Results

| Metric | Before | After | Change |
|--------|--------|-------|--------|
| Build Time (real) | 88.647s | 87.545s | -1.102s (-1.24%) |
| Build Time (user) | 465.15s | 459.81s | -5.34s |
| Build Time (sys) | 134.18s | 129.69s | -4.49s |
| Crates Compiled | 370 | 370 | 0 |

**Incremental build (touch main.rs)**: ~5.9s

### Analysis

#### Why Such Small Improvement?

1. **tokio "full" features**: While "full" includes extra features, they don't significantly increase compilation time since:
   - Most features just enable more code in the tokio crate itself
   - They don't pull in many additional dependencies
   - Impact: ~1s saved

2. **axum duplication still present**: 
   ```
   axum v0.7.9 ← tonic ← opentelemetry-otlp ← dwctl
   axum v0.8.6 ← dwctl (direct)
   ```
   - The 0.7.9 version comes from **tonic** (gRPC library)
   - tonic is pulled in by opentelemetry-otlp
   - Fixing fusillade's dev-dependencies didn't help because it doesn't affect production builds
   - Both axum versions are STILL being compiled

### Key Finding

**The real bottleneck is OpenTelemetry**:
- Pulls in tonic (gRPC) → which pulls in axum 0.7.9
- Adds significant compile-time dependencies
- Used in production, so can't be removed entirely

## Recommended Next Steps

### Option A: Make OpenTelemetry Optional (High Impact)
Move opentelemetry behind a feature flag:
```toml
[features]
default = []
telemetry = ["dep:opentelemetry", "dep:opentelemetry_sdk", "dep:opentelemetry-otlp"]
```

**Expected impact**: 
- Dev builds without telemetry: ~60-70s (20-30% faster)
- Production builds: same as current

### Option B: Check for tonic/axum Compatibility
- Wait for tonic to update to axum 0.8
- Or use tonic's http feature without axum dependency

### Option C: Make Embedded DB Default
Since --no-default-features is used for testing, embedded-db doesn't affect our benchmarks.
Default features don't matter for this analysis.

### Option D: Replace Build-Time Macros
- sqlx: Use runtime queries instead of compile-time checked queries
- utoipa: Generate OpenAPI at runtime instead of compile-time
- **High risk**: Loses type safety and compile-time guarantees

## Conclusion

Phase 1 optimizations achieved minimal improvement (1.24%) because:
1. The actual duplicate axum comes from tonic, not fusillade
2. tokio "full" features don't add many dependencies

**To achieve 20-40% improvement, we need to make OpenTelemetry optional for dev builds.**

This would eliminate:
- tonic
- prost (protobuf)
- axum 0.7.9 duplication
- All opentelemetry dependencies

Estimated time saved: **20-30 seconds** (22-34% improvement)
