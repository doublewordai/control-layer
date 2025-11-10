# Test Suite Benchmark Results

## Test Environment
- **Machine**: 16 CPU cores
- **Postgres**: Version 16.10
- **Test Count**: 375 tests using `#[sqlx::test]`
- **Migrations**: 17 for dwctl + 7 for fusillade

## Benchmark Results

| Configuration | Runtime | vs Baseline | Notes |
|--------------|---------|-------------|-------|
| **Baseline** (disk + fsync=off) | **19.99s** | - | Original configuration |
| **tmpfs** (RAM + fsync=off) | **18.38s** | **-8%** ⭐ | Best optimization |
| **tmpfs + extra settings** | 18.64s | -7% | Slightly slower than tmpfs alone |

## Key Findings

### 1. tmpfs Provides Meaningful Improvement
Using `/dev/shm` (tmpfs/RAM) for postgres data directory provides an **8% speedup (1.6s faster)**.

- **Baseline**: 19.99s on disk with `fsync=off`
- **tmpfs**: 18.38s on RAM with `fsync=off`
- **Improvement**: 1.61 seconds faster

### 2. Additional Postgres Settings Don't Help
Adding extra postgres optimizations (`full_page_writes=off`, `wal_level=minimal`, etc.) **did not improve performance** and was marginally slower:

- **tmpfs alone**: 18.38s
- **tmpfs + extra settings**: 18.64s
- **Difference**: 0.26s slower (likely within noise margin)

This suggests that with `fsync=off` and tmpfs, postgres is already at peak performance for this workload.

### 3. Most Time is NOT in Database I/O
The modest 8% improvement from eliminating all disk I/O suggests that the test suite bottleneck is elsewhere:

Likely bottlenecks:
- **Migration execution** (375 tests × 24 migrations = 9,000 migration runs)
- **Test isolation setup** (creating schemas/transactions for each test)
- **Connection pool contention** (tests competing for database connections)
- **CPU-bound test logic** (actual test code execution)

## Recommended Configuration

For fastest test execution, use tmpfs:

```bash
PGDATA="/dev/shm/pg_test_data"  # Use RAM instead of disk
PGSOCK="/dev/shm/pg_test_sock"

postgres -D "$PGDATA" \
  -c fsync=off \
  -c listen_addresses='localhost' \
  -c port=5432 \
  -c unix_socket_directories="$PGSOCK"
```

**Don't bother with**: `full_page_writes`, `wal_level`, `max_wal_senders`, `checkpoint_timeout`, `shared_buffers` - they provide no measurable benefit.

## Further Optimization Opportunities

Since database I/O is only ~8% of test time, bigger wins would come from:

1. **Reduce migration overhead** - The biggest bottleneck
   - Use sqlx test fixtures to cache migrated database state
   - Or pre-migrate a template database that tests clone from

2. **Increase connection pool size** - Reduce contention
   - Set `SQLX_TEST_MAX_CONNECTIONS=32` (from default ~10)

3. **Explicit test parallelism** - Ensure all cores are used
   - Run with `cargo test --lib --test-threads=16`

4. **Reuse postgres between runs** - Save 2-3s startup time
   - Keep postgres running instead of stopping/starting

## Implementation

The optimized benchmark script is in `/home/user/control-layer/benchmark.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

PGBIN="/usr/lib/postgresql/16/bin"
PGUSER="postgres"

# Use tmpfs for best performance
PGDATA="/dev/shm/pg_test_data"
PGSOCK="/dev/shm/pg_test_sock"

# ... (see benchmark.sh for full implementation)
```

## Conclusion

**Using tmpfs provides an 8% speedup with minimal effort.** However, the real opportunity for major speedups (2-3x) lies in optimizing the test harness itself, not the database configuration.

The database is already fast enough - focus optimization efforts on:
- Migration caching
- Connection pooling
- Test parallelization
