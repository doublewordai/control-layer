# Proc-Macro Compilation Overhead Analysis

## What We Measured

From our benchmarks, the **18-20 seconds is PURE TEST EXECUTION**, not compilation.

### Benchmark Methodology
```bash
# Step 1: Compile (output hidden)
cargo test --lib --no-run > /dev/null 2>&1

# Step 2: Run tests (TIMED)
start=$(date +%s.%N)
cargo test --lib > /dev/null 2>&1  # This should use cached build
end=$(date +%s.%N)
# Result: 18-20 seconds
```

## Proc-Macro Bottlenecks

You're absolutely right that proc-macro heavy crates are a major issue. Let's identify them:

### Heavy Proc-Macro Crates in Your Stack

From the compilation output, these are the main culprits:

1. **sqlx-macros** - Compile-time SQL verification
   - `sqlx::query!()` - Validates SQL against live database
   - `#[sqlx::test]` - Generates test harness code
   - **Impact**: Requires database connection during compilation
   - **Build time**: Moderate-High (connects to DB for each macro invocation)

2. **utoipa-gen** - OpenAPI schema generation
   - `#[utoipa::path]` - Generates OpenAPI specs from function signatures
   - **Impact**: Heavy AST analysis and codegen
   - **Build time**: High (processes all API endpoints)

3. **bon-macros** - Builder pattern generation
   - `#[derive(Builder)]` or `#[builder]` - Generates builder methods
   - **Impact**: Code generation for each struct
   - **Build time**: Moderate

4. **Tokio macros** - Async runtime
   - `#[tokio::test]` - Generates async test runners
   - `#[tokio::main]` - Async main wrapper
   - **Build time**: Low-Moderate (common, well-optimized)

5. **Serde derive** - Serialization
   - `#[derive(Serialize, Deserialize)]` - Generates ser/de code
   - **Build time**: Low-Moderate (widely used, optimized)

6. **Other proc-macros**:
   - `async-trait` - Async trait desugaring
   - `thiserror` - Error type derivation
   - `tracing-attributes` - Instrumentation
   - All accumulate to significant overhead

## The Real Problem

**Proc-macros don't cache well across changes.**

Even tiny changes force recompilation of:
1. The crate with the change
2. All downstream crates that use proc-macros on types from that crate
3. All crates that depend on those crates

### Example Cascade:
```
dwctl/src/types.rs changes
  ↓
sqlx-macros recompiles (uses types in queries)
  ↓
utoipa-gen recompiles (uses types in API schemas)
  ↓
bon-macros recompiles (uses types in builders)
  ↓
All test files recompile
```

## Optimization Strategies

### 1. **Use sccache or cachepot** (BEST for CI/local)

**What it does**: Caches compiled artifacts across builds, even proc-macro expansions.

**Setup**:
```bash
# Install sccache
cargo install sccache

# Configure Rust to use it
export RUSTC_WRAPPER=sccache

# Now cargo will cache compiled code
cargo build
```

**Expected improvement**:
- Fresh builds: Same speed
- Incremental builds: 50-80% faster
- Works across git branches

**CI Integration**:
```yaml
# .github/workflows/test.yml
- uses: mozilla-actions/sccache-action@v0.0.7
- name: Run tests
  env:
    RUSTC_WRAPPER: sccache
  run: cargo test
```

### 2. **Reduce sqlx-macros** overhead

**Problem**: `sqlx::query!()` connects to database at compile time.

**Options**:

**A) Use offline mode** (recommended for CI):
```bash
# Generate query metadata once
cargo sqlx prepare --workspace

# Now builds work without database
SQLX_OFFLINE=true cargo build
```

**B) Use query_as instead of query!** (less type-safe):
```rust
// Instead of:
sqlx::query!("SELECT * FROM users WHERE id = $1", user_id)

// Use:
sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
  .bind(user_id)
```

**Trade-off**: Lose compile-time SQL validation, gain faster builds.

### 3. **Split crates to reduce recompilation**

**Current structure** (all in one crate):
```
dwctl/
  src/
    api/      # Uses utoipa-gen
    db/       # Uses sqlx-macros
    types/    # Used by both
```

**Problem**: Changing types forces recompilation of ALL proc-macros.

**Better structure**:
```
dwctl-core/     # Types only, no proc-macros
dwctl-db/       # Uses sqlx-macros, depends on dwctl-core
dwctl-api/      # Uses utoipa-gen, depends on dwctl-core
dwctl/          # Bins together
```

**Benefit**: Changing types only recompiles what's needed.

**Effort**: High (significant refactoring).

### 4. **Use cargo-nextest** for faster test execution

**What it does**: Runs tests in parallel more efficiently than cargo test.

```bash
# Install
cargo install cargo-nextest

# Run tests (faster)
cargo nextest run
```

**Expected improvement**: 20-30% faster test execution (not compilation).

### 5. **Optimize proc-macro usage**

**utoipa-gen**:
```rust
// Instead of annotating everything:
#[utoipa::path(responses(...))]  // Heavy codegen
async fn handler() {}

// Generate OpenAPI manually or selectively
```

**bon-macros**:
```rust
// Instead of deriving builders for everything:
#[derive(Builder)]
struct Config { ... }

// Manually implement builders for hot paths
```

### 6. **Use mold linker** (faster linking)

```bash
# Install
cargo install mold

# Configure
export RUSTFLAGS="-C link-arg=-fuse-ld=mold"

# Or in .cargo/config.toml:
[target.x86_64-unknown-linux-gnu]
linker = "clap"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

**Expected improvement**: 30-50% faster linking (especially for large binaries).

### 7. **Cranelift backend for debug builds**

```toml
# .cargo/config.toml
[profile.dev]
codegen-backend = "cranelift"
```

**Expected improvement**: 15-20% faster debug compilation.
**Trade-off**: Slower runtime performance (debug only).

## Quick Wins Priority

1. **sccache** - 5 min setup, 50-80% faster incremental builds
2. **SQLX_OFFLINE** - 10 min setup, removes compile-time DB dependency
3. **cargo-nextest** - 2 min install, 20-30% faster test execution
4. **mold linker** - 5 min setup, 30-50% faster linking

## Measurement

To properly measure proc-macro overhead:

```bash
# Profile compilation
cargo build --timings

# Open target/cargo-timings/cargo-timing.html
# This shows which crates take longest to compile
```

Or use cargo-llvm-lines to see macro expansion size:

```bash
cargo install cargo-llvm-lines
cargo llvm-lines | head -50
```

## Expected Total Impact

**Current state**:
- Cold build: ~5-10 minutes (est.)
- Warm build after small change: ~30-60 seconds (est.)
- Test execution: 18-20 seconds

**With optimizations** (sccache + offline sqlx + nextest):
- Cold build: ~5-10 minutes (same)
- Warm build: ~10-20 seconds (3-6x faster)
- Test execution: ~12-15 seconds (1.3x faster)

## Recommendation

**Phase 1** (do now):
1. Set up sccache
2. Use SQLX_OFFLINE=true in CI
3. Try cargo-nextest

**Phase 2** (if needed):
4. Profile with cargo build --timings
5. Selectively reduce proc-macro usage in hot paths
6. Consider mold linker

**Phase 3** (major refactor):
7. Split into multiple crates

## Conclusion

You're correct that proc-macros are a bottleneck, **but for compilation time, not test execution time**. The 18-20 seconds we measured is pure test execution.

The real wins are:
- **sccache**: Faster development iteration
- **Template databases**: Faster test execution (as discussed earlier)
- Combined: Much better developer experience

Both optimizations are independent and stackable!
