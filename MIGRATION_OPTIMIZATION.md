# Migration Optimization Analysis

## Current Situation

### Migration Overhead
- **dwctl migrations**: 25 files, ~747 lines of SQL
- **fusillade migrations**: 7 files, ~254 lines of SQL
- **Total migrations**: 32 separate migration files
- **Total tests**: ~375 tests using `#[sqlx::test]`
- **Migration executions per test run**: 375 tests × 32 migrations = **12,000 migration operations**

### How sqlx::test Works

The `#[sqlx::test]` macro:
1. Creates a new isolated database for each test (with random name)
2. Runs ALL migrations in the migrations folder
3. Provides a PgPool to the test
4. Drops the database after the test completes

This ensures test isolation but has significant overhead.

## Why This is Slow

Based on our benchmarking:
- **Current test time**: ~18-20 seconds
- **Database I/O**: Only ~8% of total time (tmpfs vs disk showed 1.6s difference)
- **The real bottleneck**: Creating databases + running migrations **375 times**

The overhead breakdown per test:
1. `CREATE DATABASE test_xxx` - ~10-50ms
2. Run 32 migrations - ~50-200ms depending on complexity
3. Test execution - varies
4. `DROP DATABASE test_xxx` - ~10-50ms

Multiply by 375 tests = significant overhead!

## Optimization Strategies

### Option 1: Use Template Databases (BEST - 5-10x faster)

**What it is**: The `sqlx-pg-test-template` crate uses PostgreSQL's `CREATE DATABASE ... WITH TEMPLATE` feature to clone a pre-migrated database instead of running migrations for each test.

**How it works**:
```rust
// Instead of:
#[sqlx::test]
async fn my_test(pool: PgPool) { ... }

// Use:
#[sqlx_pg_test_template::test(template = "dwctl_test_template")]
async fn my_test(pool: PgPool) { ... }
```

**Setup**:
1. Add dependency to `Cargo.toml`:
   ```toml
   [dev-dependencies]
   sqlx-pg-test-template = "0.3"
   ```

2. Create template databases once (manually or in build script):
   ```sql
   CREATE DATABASE dwctl_test_template;
   -- Run all migrations once
   ```

3. Each test clones the template (very fast in Postgres):
   ```sql
   CREATE DATABASE test_random_name WITH TEMPLATE dwctl_test_template;
   ```

**Performance**:
- Creating database with template: ~5-15ms
- Running 32 migrations: ~50-200ms
- **Speedup**: 10-40x faster per test
- **Estimated total improvement**: 5-10 seconds saved (25-50% faster tests)

**Pros**:
- ✅ Fastest option by far
- ✅ No changes to test code (just macro replacement)
- ✅ Maintains migration history for production
- ✅ Easy to update (just recreate template when migrations change)

**Cons**:
- ❌ Need to maintain template database
- ❌ Adds another dependency
- ❌ Template needs refresh when migrations change (could automate)

### Option 2: Squash Migrations (GOOD - 2-3x faster)

**What it is**: Combine all 32 migration files into 1 large migration file.

**How to do it**:
1. Apply all current migrations to a database
2. Use `pg_dump --schema-only` to get the final schema
3. Replace migrations folder with single `001_initial.sql`

**Performance**:
- 375 tests × 1 migration instead of 375 × 32
- **Speedup**: 2-3x faster on migration overhead
- **Estimated improvement**: 3-5 seconds saved

**Pros**:
- ✅ No new dependencies
- ✅ Works with standard `#[sqlx::test]`
- ✅ Simpler migrations folder

**Cons**:
- ❌ Lose migration history (can keep in separate branch/tag)
- ❌ Harder to rollback in production
- ❌ Need to manually squash after each migration change
- ❌ Team members used to incremental migrations may resist

### Option 3: Use Fixtures Instead of Migrations (GOOD - 2-3x faster)

**What it is**: Disable automatic migrations and use a fixture file with the complete schema.

**How it works**:
```rust
#[sqlx::test(migrations = false, fixtures("schema"))]
async fn my_test(pool: PgPool) { ... }
```

Create `fixtures/schema.sql` with the full schema (from `pg_dump`).

**Performance**:
- Same as squashing (1 SQL file vs 32)
- **Estimated improvement**: 3-5 seconds saved

**Pros**:
- ✅ No new dependencies
- ✅ Keeps migration history for production
- ✅ Clear separation between prod migrations and test schema

**Cons**:
- ❌ Need to maintain separate schema file
- ❌ Easy to forget to update fixture when migrations change
- ❌ Requires changing all test annotations

### Option 4: Selective Migration Optimization (MODERATE)

**What it is**: Analyze which migrations are expensive and optimize them for test environments.

**Example optimizations**:
- Disable indexes during test (recreate at end)
- Skip data migrations in tests (if not needed)
- Use UNLOGGED tables in tests (faster writes, no WAL)

**Performance**:
- Variable, depends on migration complexity
- **Estimated improvement**: 1-3 seconds

**Pros**:
- ✅ No architectural changes
- ✅ Can be done incrementally

**Cons**:
- ❌ Complex to maintain
- ❌ Tests might not match production exactly
- ❌ Limited impact (migrations are already relatively simple)

## Recommended Approach

### Phase 1: Quick Win - Use Template Databases

1. **Add sqlx-pg-test-template** to dev-dependencies
2. **Create template database setup script**:
   ```bash
   #!/usr/bin/env bash
   # setup-test-template.sh

   createdb dwctl_test_template
   createdb fusillade_test_template

   # Run migrations
   DATABASE_URL="postgres://postgres@localhost/dwctl_test_template" \
     sqlx migrate run --source dwctl/migrations

   DATABASE_URL="postgres://postgres@localhost/fusillade_test_template" \
     sqlx migrate run --source fusillade/migrations
   ```

3. **Replace test macros** (can be done incrementally):
   ```rust
   // Old:
   #[sqlx::test]

   // New:
   #[sqlx_pg_test_template::test(template = "dwctl_test_template")]
   ```

4. **Update benchmark script** to create templates before running tests

**Expected impact**: 5-10 second improvement (25-50% faster)

### Phase 2: If Needed - Squash Old Migrations

Once you have 50+ migrations, squash older ones periodically:
1. Keep recent migrations (last 6 months) as separate files
2. Squash older migrations into a single "baseline" migration
3. This keeps the benefits of migration history while reducing count

## Migration Transaction Behavior

**Q: Do migrations require transactions?**

**A**: It depends on the operations:

- **DDL operations in Postgres**: Most DDL (CREATE TABLE, ALTER TABLE, etc.) IS transactional in Postgres (unlike MySQL/others)
- **sqlx migrations**: Each migration file is wrapped in a transaction by default
- **Atomicity**: If a migration fails, it rolls back automatically

**For test optimization**:
- Transactions are actually HELPFUL for tests (faster than commits)
- sqlx::test uses transactions for test isolation anyway
- Template databases are still faster because they skip the operations entirely

**Squashing concerns**:
- No need to worry about transaction boundaries
- All DDL in Postgres is transactional
- Can safely combine migrations into one file

## Implementation Priority

1. ✅ **DONE**: tmpfs for database storage (8% improvement)
2. 🎯 **NEXT**: Template databases via sqlx-pg-test-template (25-50% improvement)
3. 📅 **LATER**: Squash migrations when count > 50

## Template Database Maintenance

**Keeping template fresh**:

Option A - Manual (simplest):
```bash
# When migrations change:
dropdb dwctl_test_template --if-exists
createdb dwctl_test_template
sqlx migrate run --source dwctl/migrations
```

Option B - Automated in test setup:
```rust
// In tests/common/mod.rs
fn ensure_template_fresh() {
    // Check if template exists and is up to date
    // Recreate if needed
}
```

Option C - CI/CD:
```yaml
# .github/workflows/test.yml
- name: Setup test templates
  run: ./scripts/setup-test-templates.sh
```

## Conclusion

**Recommendation**: Use `sqlx-pg-test-template` for 5-10x faster migration overhead.

Combined with tmpfs (already tested), expected total speedup:
- **Before**: ~20 seconds
- **After**: ~10-12 seconds
- **Improvement**: 40-50% faster test suite

This is the biggest optimization you can make without changing test architecture.
