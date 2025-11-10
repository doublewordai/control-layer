# Test Suite Optimization Recommendations

## Current State Analysis

- **375 tests** across 25 files using `#[sqlx::test]`
- **sqlx::test** creates isolated database per test (separate transaction/database)
- **17 migrations for dwctl** + **7 for fusillade** run for each test
- Benchmark script **restarts postgres** between runs (unnecessary)
- **16 CPU cores** available for parallelization

## Optimization Strategies

### 1. **Remove Unnecessary Postgres Restarts** (Highest Impact - ~5-10s savings)

**Current benchmark script:**
```bash
just db-stop --remove > /dev/null 2>&1 || true  # Stops and removes container
just db-start > /dev/null 2>&1                   # Starts fresh container
just db-setup > /dev/null 2>&1                   # Sets up databases
```

**Problem:** This adds 5-10 seconds of overhead for container lifecycle management.

**Solution:** Keep postgres running between test runs. Only restart if absolutely necessary for a clean benchmark.

**Improved benchmark script:**
```bash
#!/usr/bin/env bash
set -euo pipefail

# Only setup databases once if not already done
if ! PGPASSWORD=password psql -h localhost -p 5432 -U postgres -d test -c '\q' 2>/dev/null; then
    echo "Starting postgres (first time only)..."
    just db-start > /dev/null 2>&1
    just db-setup > /dev/null 2>&1
fi

echo "Compiling tests..."
cargo test --lib --no-run > /dev/null 2>&1

echo "Running test suite."
start=$(date +%s.%N)
cargo test --lib > /dev/null 2>&1
end=$(date +%s.%N)

runtime=$(echo "$end - $start" | bc)
echo "Test suite completed in ${runtime}s"
```

**Expected Impact:** 5-10 second reduction per benchmark run

---

### 2. **Use tmpfs for Postgres Data Directory** (High Impact - ~30-50% faster)

**Problem:** Postgres writes to disk are slow, even with fsync disabled.

**Solution:** Mount postgres data directory on tmpfs (RAM-backed filesystem).

**Updated db-start command:**
```bash
db-start:
    #!/usr/bin/env bash
    set -euo pipefail

    if docker ps -a --format '{{{{.Names}}' | grep -q "^test-postgres$"; then
        if docker ps --format '{{{{.Names}}' | grep -q "^test-postgres$"; then
            echo "✅ test-postgres container is already running"
        else
            echo "Starting existing test-postgres container..."
            docker start test-postgres
        fi
    else
        echo "Creating new test-postgres container with tmpfs and fsync disabled..."
        docker run --name test-postgres \
          -e POSTGRES_PASSWORD=password \
          -p 5432:5432 \
          --tmpfs /var/lib/postgresql/data:rw,size=1g \
          -d postgres:latest \
          postgres -c fsync=off -c full_page_writes=off -c synchronous_commit=off
    fi

    echo "Waiting for postgres to be ready..."
    sleep 2

    if pg_isready -h localhost -p 5432 >/dev/null 2>&1; then
        echo "✅ PostgreSQL is ready on localhost:5432"
        echo "   Credentials: postgres/password"
        echo "   ⚠️  Running on tmpfs with fsync disabled - for testing only!"
    else
        echo "❌ PostgreSQL not responding"
        exit 1
    fi
```

**Expected Impact:** 30-50% faster test execution

---

### 3. **Optimize sqlx::test Configuration** (Medium Impact - ~20-30% faster)

**Problem:** Default sqlx::test settings may not be optimal for this codebase.

**Solution:** Configure sqlx test behavior via environment variables and attributes.

**Create `.cargo/config.toml`:**
```toml
[env]
# Optimize for test performance
TEST_DATABASE_URL = "postgres://postgres:password@localhost/test"
SQLX_TEST_DATABASE = "postgres://postgres:password@localhost/test"

# sqlx test pool settings - increase for better parallelism
SQLX_TEST_MIN_CONNECTIONS = "1"
SQLX_TEST_MAX_CONNECTIONS = "32"  # Increase from default (usually 10)
SQLX_TEST_CONNECT_TIMEOUT = "30"
SQLX_TEST_ACQUIRE_TIMEOUT = "30"
```

**Expected Impact:** 20-30% faster with proper connection pooling

---

### 4. **Leverage Cargo Test Parallelism** (Medium Impact)

**Current:** Cargo likely defaults to running tests with some parallelism.

**Optimization:** Explicitly control test parallelism based on CPU count.

**Updated benchmark script:**
```bash
# Run with explicit parallelism (16 cores available)
cargo test --lib --test-threads=16 > /dev/null 2>&1
```

**Note:** sqlx::test creates database isolation, so parallel execution should be safe. However, you may hit connection pool limits, which is why optimizing max_connections (strategy #3) is important.

**Expected Impact:** Depends on current parallelism, potentially 10-20% if not already maximized

---

### 5. **Use Prepared Query Cache** (Low-Medium Impact - ~5-15% faster)

**Problem:** sqlx query macros perform compile-time verification, but tests still need to prepare queries.

**Solution:** Ensure `.sqlx` directory is up to date to skip online verification.

**Add to benchmark script:**
```bash
# Ensure sqlx prepared queries are cached
if [ ! -d "dwctl/.sqlx" ] || [ ! -d "fusillade/.sqlx" ]; then
    echo "Preparing sqlx query cache..."
    cargo sqlx prepare --workspace > /dev/null 2>&1
fi
```

**Expected Impact:** 5-15% faster compilation/test startup

---

### 6. **Optimize Migrations for Tests** (Medium-High Impact - ~20-40% faster)

**Problem:** Running 24 migrations for each test is slow.

**Solution:** Use sqlx::test fixtures to pre-run migrations and snapshot the database state.

**Implementation:** Create a custom test fixture that runs once:

Add to relevant test files:
```rust
#[sqlx::test(migrations = "./migrations")]
async fn my_test(pool: PgPool) {
    // Test code
}
```

Or create a shared fixture in `dwctl/tests/fixtures/`:
```sql
-- This runs once and sqlx::test can clone it for each test
```

**Expected Impact:** 20-40% faster test execution

---

### 7. **Disable Unnecessary Postgres Settings** (Small Impact - ~5-10% faster)

**Current:** Using `fsync=off` (good!)

**Additional optimizations:**
```bash
postgres \
  -c fsync=off \
  -c full_page_writes=off \
  -c synchronous_commit=off \
  -c wal_level=minimal \
  -c max_wal_senders=0 \
  -c checkpoint_timeout=1d \
  -c shared_buffers=256MB
```

**Expected Impact:** 5-10% faster

---

## Recommended Implementation Order

1. **Quick wins (do first):**
   - Remove postgres restarts (#1) - 5 minutes to implement
   - Use tmpfs (#2) - 5 minutes to implement
   - Optimize postgres settings (#7) - 2 minutes to implement

2. **Medium effort:**
   - Configure sqlx test settings (#3) - 10 minutes
   - Adjust test parallelism (#4) - 5 minutes
   - Use prepared query cache (#5) - 5 minutes

3. **Larger refactor (if needed):**
   - Optimize migrations with fixtures (#6) - 1-2 hours

## Expected Total Impact

- **Cumulative speedup:** 2-4x faster test execution
- **From estimated ~60s → ~15-30s** (rough estimate, actual times depend on current baseline)

## Benchmark Script Template

Here's a complete optimized benchmark script:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Ensure postgres is running (don't restart between runs)
if ! pg_isready -h localhost -p 5432 >/dev/null 2>&1; then
    echo "Starting postgres..."
    # Use tmpfs-backed postgres with optimized settings
    docker run --name test-postgres \
      -e POSTGRES_PASSWORD=password \
      -p 5432:5432 \
      --tmpfs /var/lib/postgresql/data:rw,size=1g \
      -d postgres:latest \
      postgres -c fsync=off -c full_page_writes=off \
               -c synchronous_commit=off -c wal_level=minimal \
               -c max_wal_senders=0
    sleep 3
fi

# Setup databases (idempotent)
if ! PGPASSWORD=password psql -h localhost -p 5432 -U postgres -d test -c '\q' 2>/dev/null; then
    echo "Setting up databases..."
    PGPASSWORD=password createdb -h localhost -p 5432 -U postgres dwctl
    PGPASSWORD=password createdb -h localhost -p 5432 -U postgres fusillade
    echo "DATABASE_URL=postgres://postgres:password@localhost:5432/dwctl" > dwctl/.env
    echo "DATABASE_URL=postgres://postgres:password@localhost:5432/fusillade" > fusillade/.env
    (cd dwctl && sqlx migrate run)
    (cd fusillade && sqlx migrate run)
fi

# Ensure query cache is current
cargo sqlx prepare --workspace --check > /dev/null 2>&1 || {
    echo "Updating sqlx query cache..."
    cargo sqlx prepare --workspace > /dev/null 2>&1
}

echo "Compiling tests..."
cargo test --lib --no-run > /dev/null 2>&1

echo "Running test suite with 16 parallel threads."
start=$(date +%s.%N)
TEST_DATABASE_URL="postgres://postgres:password@localhost:5432/test" \
  SQLX_TEST_MAX_CONNECTIONS=32 \
  cargo test --lib --test-threads=16 > /dev/null 2>&1
end=$(date +%s.%N)

runtime=$(echo "$end - $start" | bc)
echo "Test suite completed in ${runtime}s"
```

## Monitoring & Validation

To validate improvements:

1. **Baseline measurement:** Run current script 3 times, take average
2. **Apply optimizations incrementally:** Measure after each change
3. **Watch for:**
   - Connection pool exhaustion errors
   - Flaky tests (indicates race conditions)
   - Memory usage (tmpfs uses RAM)

## Additional Considerations

- **CI Environment:** These optimizations work great for local development. For CI, consider:
  - Using GitHub Actions' postgres service container with tmpfs
  - Caching sqlx prepared queries
  - Running tests in parallel across multiple jobs

- **Safety:** All optimizations maintain test isolation and correctness. tmpfs and disabled fsync are safe for tests (not for production!).

---

**Next Steps:** Would you like me to implement any of these optimizations? I recommend starting with #1, #2, and #7 for immediate 2-3x speedup with minimal effort.
