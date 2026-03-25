# Per-User Fair Scheduling in Fusillade

## Context

Fusillade currently schedules requests globally by batch deadline (`expires_at ASC`). A single high-volume user can monopolise model capacity. The "Enabling Background Agents" project requires per-user fair scheduling so throughput is distributed evenly across users (TTFT guarantee).

Linear issues: COR-226 (parent), COR-227, COR-228, COR-229, COR-230.

## Key Insight: No New Column Needed

The `claim_requests` query already joins `batches b` and returns `b.created_by` (line 887 of `postgres.rs`). The `active_batch_ids` CTE just needs to include `created_by` and the ordering needs to partition by it. No migration to add `user_id` to `requests`.

---

## Implementation Order

### Step 1: COR-227 — Ensure `created_by` is reliable

**Goal:** Make `batches.created_by` non-nullable so the scheduler always has a user key.

**Files:**
- `migrations/` — New migration:
  ```sql
  UPDATE batches SET created_by = '' WHERE created_by IS NULL;
  ALTER TABLE batches ALTER COLUMN created_by SET NOT NULL;
  ALTER TABLE batches ALTER COLUMN created_by SET DEFAULT '';
  ```
- `src/batch/mod.rs` lines 364, 412, 438 — Keep `BatchInput.created_by` as `Option<String>` for API backwards compat, but change `Batch.created_by` to `String`
- `src/manager/postgres.rs` — Update `batch_from_dynamic_row!` macro (line ~106) and `create_batch` INSERT (line ~2118) to coalesce `None` → `""`

**Breaking change:** `Batch.created_by: Option<String>` → `String`. This is a minor breaking change for downstream consumers that pattern-match on it.

---

### Step 2: COR-228 — Rewrite `claim_requests` for per-user fair scheduling

**Goal:** Round-robin across users within each model's capacity, maintaining deadline ordering within each user.

**Files:**
- `src/manager/postgres.rs` — Rewrite the SQL in `claim_requests` (lines 829-893)

**Approach:** Use `ROW_NUMBER() OVER (PARTITION BY created_by ORDER BY expires_at)` to rank each user's requests, then order by rank (interleaving users). Join back to `requests` for `FOR UPDATE SKIP LOCKED`:

```sql
WITH active_batch_ids AS MATERIALIZED (
    SELECT b.id, b.expires_at, b.created_by
    FROM batches b
    WHERE b.cancelling_at IS NULL
        AND b.deleted_at IS NULL
        AND b.completed_at IS NULL
        AND b.failed_at IS NULL
        AND b.cancelled_at IS NULL
        AND EXISTS (
            SELECT 1 FROM requests r
            WHERE r.batch_id = b.id AND r.state = 'pending'
        )
),
ranked AS (
    SELECT r3.id, r3.model,
           ROW_NUMBER() OVER (
               PARTITION BY ab.created_by, r3.model
               ORDER BY ab.expires_at ASC, ab.id ASC, r3.id ASC
           ) as user_rank
    FROM active_batch_ids ab
    JOIN requests r3 ON r3.batch_id = ab.id
    WHERE r3.state = 'pending'
        AND r3.template_id IS NOT NULL
        AND (r3.not_before IS NULL OR r3.not_before <= $3)
),
to_claim AS (
    SELECT claimed.id, claimed.template_id, claimed.batch_id
    FROM unnest($4::TEXT[], $5::BIGINT[]) AS m(model, capacity)
    CROSS JOIN LATERAL (
        SELECT r.id, r.template_id, r.batch_id
        FROM ranked rk
        JOIN requests r ON r.id = rk.id
        WHERE rk.model = m.model
        ORDER BY rk.user_rank ASC  -- interleave users, deadline order within each user
        LIMIT m.capacity
        FOR UPDATE OF r SKIP LOCKED
    ) claimed
    LIMIT $2::BIGINT
)
UPDATE requests r SET ...
```

**Testing:**
- New integration test: 3 users with varying request counts, verify interleaved claiming
- Existing deadline-ordering tests should still pass for single-user scenarios

---

### Step 3: COR-229 — Per-user in-flight tracking

**Goal:** Track per-user in-flight counts; optionally enforce per-user concurrency limits.

**Files:**
- `src/daemon/mod.rs`:
  - Add `user_requests_in_flight: Arc<DashMap<String, AtomicUsize>>` to `Daemon` struct (line ~275)
  - Add `per_user_concurrency_limit: Option<usize>` to `DaemonConfig` (line ~78), default `None`
  - Extract owner from `request.data.batch_metadata.get("created_by")` before spawning (line ~684). Add `"created_by"` to `default_batch_metadata_fields()` (line 221).
  - Increment/decrement `user_requests_in_flight` in spawn/scopeguard (lines 712-750)
- `src/manager/mod.rs` line 292 — Add `per_user_limit: Option<usize>` param to `Storage::claim_requests` trait
- `src/manager/postgres.rs` — Add `WHERE rk.user_rank <= $6` when `per_user_limit` is `Some`

**Breaking change:** `Storage::claim_requests` signature change. This is a trait method on a public trait — major version bump.

**Alternative to avoid breaking change:** Don't change the trait. Instead, add `"created_by"` to default metadata fields and enforce per-user limits purely in the daemon by tracking counts and passing adjusted capacity. The claim query already does per-user interleaving from Step 2. The daemon can skip spawning if a user exceeds their limit, returning the request to pending. Less efficient (wastes claim slots) but avoids the trait break.

**Recommendation:** Go with the trait change. We're already doing a `feat!:` for COR-227. Bundle the breaking changes into one major version.

---

### Step 4: COR-230 — Per-user throughput via OTel

**Goal:** Emit per-user throughput from the daemon's processing loop via structured logs.

**Files:**
- `src/daemon/mod.rs`:
  - Add `user_throughput: Arc<DashMap<String, UserThroughputStats>>` to `Daemon`
  - New struct `UserThroughputStats { completed: AtomicU64, failed: AtomicU64, last_reset: Instant }`
  - On request completion (lines 797-818), increment the user's counter
  - New periodic task (alongside heartbeat/cancellation polling): every N seconds, iterate the map and emit structured logs:
    ```rust
    tracing::info!(
        user = %user_id,
        completed = completed_count,
        failed = failed_count,
        throughput_rpm = requests_per_minute,
        window_seconds = elapsed,
        "fusillade.user_throughput"
    );
    ```
  - Reset counters after emission
  - Add `throughput_log_interval_ms: Option<u64>` to `DaemonConfig` (default: 60_000)

**No breaking changes** — purely additive.

---

## Version Strategy

Bundle COR-227 through COR-229 into a single PR with `feat!:` (major version bump). COR-230 can be a separate `feat:` PR (minor bump) since it's purely additive.

Or: ship all four as one major version release.

## Verification

1. `just db-start && just db-setup` — run migrations
2. `just test` — all existing tests pass
3. New integration tests:
   - Multi-user fair scheduling: 3 users, verify interleaved claims
   - Per-user concurrency limit: verify limit enforcement
   - Throughput logging: verify structured log emission after processing
4. `just lint` — no warnings
5. `cargo sqlx prepare` — update offline query cache
