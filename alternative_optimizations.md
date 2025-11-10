# Alternative Build Optimization Strategies
## (Maintaining Dev/Prod Parity)

Based on analysis, here are options that don't diverge environments:

### Option 1: Use Faster Linker (5-15% improvement)
**Impact**: Low risk, immediate benefit
**Time saved**: 5-13 seconds

Install mold (modern fast linker):
```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

Or lld (LLVM linker):
```toml
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
```

Linking is ~10-20% of build time. Mold is 3-5x faster than default GNU ld.

### Option 2: Workspace Dependencies (Prevent Future Regressions)
**Impact**: Prevents version divergence
**Time saved**: Minimal now, but prevents future issues

```toml
# Root Cargo.toml
[workspace.dependencies]
tokio = { version = "1.48", default-features = false }
axum = "0.8"
serde = { version = "1.0", features = ["derive"] }
# ... etc
```

Then in dwctl/fusillade:
```toml
tokio = { workspace = true, features = ["macros", "rt-multi-thread", ...] }
```

This ensures only ONE version of each crate across workspace.

### Option 3: Build Configuration Tuning (2-5% improvement)
**Impact**: Low risk
**Time saved**: 2-4 seconds

```toml
# .cargo/config.toml
[build]
incremental = true

[profile.dev]
incremental = true
codegen-units = 256  # More parallelism (default 256, but explicit)

[profile.dev.package."*"]
opt-level = 0
debug = 1  # Reduced debug info for dependencies
```

### Option 4: Use sccache (Massive for repeated builds)
**Impact**: 80-95% faster on cache hit
**Time saved**: 70-80s on subsequent builds

```bash
cargo install sccache
export RUSTC_WRAPPER=sccache
```

Caches compilation artifacts across builds and even across branches.

### Option 5: Switch OTLP to HTTP-only Transport
**Status**: Already using "http-proto" + "reqwest-client"
**Problem**: Still pulls in protobuf (prost) and tonic

Current config already uses HTTP, but tonic is a transitive dependency. 
Can't eliminate without removing opentelemetry-otlp entirely.

### Option 6: Reduce sqlx Macro Usage (High Risk)
**Impact**: 10-15% faster
**Risk**: HIGH - loses compile-time SQL checking

Convert query! to query() (runtime):
```rust
// Before (compile-time)
sqlx::query!("SELECT * FROM users WHERE id = $1", id)

// After (runtime)
sqlx::query("SELECT * FROM users WHERE id = $1").bind(id)
```

**Not recommended** - compile-time checking catches bugs early.

## Recommended Approach

**Phase 2: Low-hanging fruit (5-20% improvement)**
1. Install and configure mold linker (5-15%)
2. Set up workspace dependencies (0-2%)
3. Tune build configuration (2-5%)
4. Optional: sccache for development (huge for incremental)

**Expected combined savings**: 7-22 seconds (8-25% improvement)

**Total with Phase 1**: 
- Phase 1: -1.1s
- Phase 2: -7 to -22s
- **Total: -8 to -23s (9-26% improvement)**

Target build time: **65-80 seconds** (down from 88.6s)

This achieves meaningful improvement while maintaining dev/prod parity!
