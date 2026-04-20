# BRIN Index Investigation Spec

## Goal

Determine whether BRIN indexes would improve performance and reduce index
maintenance cost for large Postgres tables in the control layer, without
regressing selective queries that are currently well served by B-tree indexes.

This document is intentionally narrow. It is not a general indexing guide. It
defines the investigation we should run against the current schema and query
patterns before changing production indexes.

## Background

BRIN indexes are most effective when all of the following are true:

- The table is large.
- Rows are inserted mostly in physical order by the indexed column.
- Queries use broad range predicates such as time windows.
- The alternative B-tree index is large enough that maintenance and storage
  cost matter.

BRIN indexes are usually a poor fit for:

- Exact-match lookups.
- Highly selective multi-column predicates.
- Small or medium tables.
- Queues or workflow tables whose hot paths depend on ordered point selection.

## Primary Questions

1. Which tables are large enough for BRIN to be worth testing?
2. For each candidate table, which columns are naturally correlated with insert
   order?
3. Do the real query patterns use broad range filters, or do they rely on
   selective point lookups where B-tree should remain primary?
4. Can BRIN be added as a supplement to existing B-tree indexes, or is there a
   credible path to replacing a specific B-tree index?

## Current Schema Candidates

### Tier 1: Strong candidate

#### `http_analytics`

Why it is a candidate:

- Large append-heavy table with `id BIGSERIAL`, `timestamp`, and `created_at`.
- Existing docs already call out 20M+ rows.
- Main analytics queries filter by time ranges and sort by `timestamp DESC`.

Relevant schema:

- `dwctl/migrations/010_http_analytics.sql`
- `docs/organizations.md`

Relevant query paths:

- Aggregations over `timestamp >= $1 AND timestamp <= $2`
- Paginated listing ordered by `timestamp DESC`

Hypothesis:

- A BRIN index on `timestamp` should help broad range scans and reduce index
  size versus the existing B-tree on `timestamp`.
- Existing selective indexes such as `(model, timestamp)` and
  `(status_code, timestamp)` will likely still be needed.

Columns to test:

- `timestamp`
- Optionally `created_at` if operational queries use it enough to matter

### Tier 2: Possible supplemental candidates

#### `credits_transactions`

Why it is a candidate:

- Append-only ledger table with `created_at`.
- Historical reporting queries may scan broad date ranges.

Why it is not a clear BRIN-first table:

- Many important reads are scoped by `user_id`.
- Balance and pagination logic depend on per-user ordering and `seq`.

Hypothesis:

- BRIN on `created_at` may help broad reporting windows.
- Existing B-tree indexes on `(user_id, created_at DESC)` and `seq`-driven
  access patterns are still expected to remain primary.

Columns to test:

- `created_at`
- Do not treat `seq` as a first BRIN target until proven necessary

#### `probe_results`

Why it is a candidate:

- Append-heavy with `executed_at`.

Why it is weak:

- Main query pattern is `WHERE probe_id = ? ORDER BY executed_at DESC`.
- That pattern usually wants a composite B-tree on `(probe_id, executed_at)`,
  not BRIN.

Hypothesis:

- BRIN on `executed_at` is unlikely to materially help the main read path.
- Investigation should confirm whether a composite B-tree gap exists instead.

## Non-Candidates

These tables should not be BRIN priorities unless new evidence appears:

### `webhook_deliveries`

Hot path is queue claiming by `status` and `next_attempt_at`, using ordered,
selective retrieval. This is a B-tree workload.

### `sync_operations`

Hot path is listing by `connection_id` with `ORDER BY created_at DESC`. This is
better served by a composite B-tree if needed.

### `sync_entries`

Hot paths are dedup and workflow lookups on `(connection_id, external_key,
external_last_modified)` and `(sync_id, status)`. These are selective indexes,
not BRIN workloads.

### `batch_capacity_reservations`

Small, short-lived reservation table with targeted lookups by `model_id`,
`completion_window`, and `expires_at`.

## Investigation Method

### 1. Capture table and index size

For each candidate table, capture:

- Table size
- Total index size
- Individual index sizes
- Estimated row count

Suggested SQL:

```sql
SELECT
  c.relname AS relation,
  c.reltuples::bigint AS est_rows,
  pg_size_pretty(pg_table_size(c.oid)) AS table_size,
  pg_size_pretty(pg_indexes_size(c.oid)) AS indexes_size,
  pg_size_pretty(pg_total_relation_size(c.oid)) AS total_size
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'public'
  AND c.relname IN ('http_analytics', 'credits_transactions', 'probe_results');
```

```sql
SELECT
  schemaname,
  tablename,
  indexname,
  pg_size_pretty(pg_relation_size(indexrelid)) AS index_size
FROM pg_indexes
JOIN pg_stat_user_indexes
  ON pg_indexes.schemaname = pg_stat_user_indexes.schemaname
 AND pg_indexes.tablename = pg_stat_user_indexes.relname
 AND pg_indexes.indexname = pg_stat_user_indexes.indexrelname
WHERE tablename IN ('http_analytics', 'credits_transactions', 'probe_results')
ORDER BY tablename, pg_relation_size(indexrelid) DESC;
```

### 2. Measure physical correlation

BRIN depends on correlation between heap order and the indexed column. Capture
`pg_stats.correlation` for each candidate column.

Suggested SQL:

```sql
SELECT
  tablename,
  attname,
  correlation
FROM pg_stats
WHERE schemaname = 'public'
  AND (
    (tablename = 'http_analytics' AND attname IN ('timestamp', 'created_at'))
    OR
    (tablename = 'credits_transactions' AND attname IN ('created_at'))
    OR
    (tablename = 'probe_results' AND attname IN ('executed_at'))
  )
ORDER BY tablename, attname;
```

Interpretation:

- Near `1.0`: strong correlation, BRIN is plausible.
- Near `0`: weak correlation, BRIN benefit is doubtful.
- Negative values: heap order is working against the target column.

### 3. Identify top query shapes

For each candidate table, collect representative queries from code and, if
available, `pg_stat_statements`.

Focus on:

- Range scans by timestamp/date
- Ordered pagination
- Per-user or per-foreign-key filtered reads
- Queue-like selective retrieval

The codebase already suggests these primary shapes:

- `http_analytics`: broad time-window aggregations and paginated listings
- `credits_transactions`: per-user history, checkpoint delta aggregation, batch
  grouping
- `probe_results`: per-probe history ordered by `executed_at DESC`

### 4. Compare plans before and after BRIN

For each candidate index, compare:

- Current plan with existing indexes
- Plan after adding BRIN
- Plan after optionally disabling competing paths in a staging session only, if
  needed to understand planner choices

Use:

```sql
EXPLAIN (ANALYZE, BUFFERS)
...;
```

Capture:

- Planning time
- Execution time
- Shared buffer hits/reads
- Heap blocks visited
- Whether the planner chose Bitmap Heap Scan, Index Scan, or Seq Scan

### 5. Evaluate write and maintenance tradeoffs

For each tested BRIN index, record:

- Index size compared with the replaced or supplemented B-tree
- Build time
- Whether `CREATE INDEX CONCURRENTLY` is required for rollout
- Autovacuum and summarization considerations

## Candidate Experiments

### Experiment A: `http_analytics(timestamp)`

Create:

```sql
CREATE INDEX CONCURRENTLY idx_http_analytics_timestamp_brin
ON http_analytics
USING BRIN (timestamp);
```

Queries to benchmark:

- Total requests in a 24-hour window
- Time-series aggregation over 7 days
- Paginated requests listing with only time bounds
- Paginated requests listing with time bounds plus low-selectivity filters

Success condition:

- BRIN materially improves or preserves range-query performance for broad
  windows while being significantly smaller than the B-tree index.

Failure condition:

- Planner ignores BRIN for the queries that matter, or broad window queries
  remain slower than the current B-tree path.

### Experiment B: `credits_transactions(created_at)`

Create:

```sql
CREATE INDEX CONCURRENTLY idx_credits_transactions_created_at_brin
ON credits_transactions
USING BRIN (created_at);
```

Queries to benchmark:

- Broad historical reporting over date ranges
- Admin views that scan large slices of transaction history
- Existing user-scoped pagination queries as a regression check

Success condition:

- Broad time-window queries improve without harming planner choices for
  user-scoped reads.

Failure condition:

- No material benefit beyond current indexes, or planner confusion causes worse
  plans for common reads.

### Experiment C: `probe_results(executed_at)`

Create only if table size justifies it and after confirming the main latency is
not caused by missing composite B-tree coverage.

Create:

```sql
CREATE INDEX CONCURRENTLY idx_probe_results_executed_at_brin
ON probe_results
USING BRIN (executed_at);
```

Queries to benchmark:

- Cross-probe reporting over large time windows
- Existing per-probe history queries

Expected outcome:

- Likely low value. This experiment is lower priority than adding a composite
  B-tree on `(probe_id, executed_at DESC)` if query plans show that gap.

## Decision Rules

Adopt a BRIN index only if all of the following hold:

- The target table is large enough that index size and maintenance cost matter.
- Column correlation is strong enough for BRIN to prune effectively.
- Measured query plans show improvement for real workloads, not just synthetic
  scans.
- The BRIN index does not cause planner regressions on the dominant selective
  queries.

Do not drop an existing B-tree index unless:

- The BRIN-backed workload is the dominant use case.
- Equivalent or better performance is confirmed for the affected queries.
- There is a clear rollback path.

## Deliverables

The investigation should produce:

- Table and index size snapshots
- Correlation results from `pg_stats`
- `EXPLAIN (ANALYZE, BUFFERS)` output for each representative query
- A recommendation table with one row per candidate:
  - `table`
  - `column`
  - `add_brin`
  - `keep_btree`
  - `drop_btree`
  - `notes`

## Initial Recommendation

Based on the schema and code inspection alone, before live measurement:

- Investigate `http_analytics(timestamp)` first.
- Investigate `credits_transactions(created_at)` second.
- Treat `probe_results(executed_at)` as low priority.
- Do not spend time on BRIN for `webhook_deliveries`, `sync_operations`,
  `sync_entries`, or `batch_capacity_reservations` unless workload evidence
  changes.

## Findings

This section summarizes the outcome of running the investigation against a
large real dataset. It intentionally avoids environment-specific details and
records only conclusions that are safe to keep in open source documentation.

### Summary

- `http_analytics(timestamp)` is the best BRIN candidate in this schema.
- `credits_transactions(created_at)` is a possible supplemental BRIN candidate
  for broad reporting workloads, but it is not the first change to make.
- `probe_results(executed_at)` is not currently a worthwhile BRIN target.
- Existing B-tree indexes should remain in place for the dominant selective and
  ordered read paths.

### `http_analytics`

Observed characteristics:

- The table behaves like a classic append-heavy event log.
- `timestamp` and `created_at` remain strongly correlated with heap order.
- Broad time-window analytics reads become expensive as the table grows.
- Ordered request listing by `timestamp DESC LIMIT ...` is still a strong
  B-tree workload.

Recommendation:

- Adding a BRIN index on `timestamp` is appropriate as a supplemental index for
  large deployments where broad time-window aggregations are important.
- Keep `idx_analytics_timestamp` even if a BRIN is added. The existing B-tree
  continues to serve ordered pagination and recent-request lookups well.
- Do not drop selective B-tree indexes such as `(model, timestamp)` or
  `(status_code, timestamp)` based on BRIN alone.

Recommended DDL when needed:

```sql
CREATE INDEX CONCURRENTLY idx_http_analytics_timestamp_brin
ON http_analytics
USING BRIN (timestamp);
```

Optional follow-up:

- If operational queries ever pivot to `created_at` rather than `timestamp`,
  evaluate a BRIN on `created_at` separately. It is not the first index to add.

### `credits_transactions`

Observed characteristics:

- `created_at` is strongly correlated with physical row order, so the table is
  technically BRIN-friendly.
- Broad history/reporting scans over time ranges become expensive as the table
  grows.
- The most important application paths are still not broad time-range scans:
  they are per-user history, recent transaction listing, and checkpoint-based
  balance calculations.
- Those dominant paths are still fundamentally B-tree-shaped, especially where
  reads are scoped by `user_id` or ordered by `seq`.

Recommendation:

- Do not treat BRIN on `created_at` as a default change for this table.
- A BRIN on `created_at` is reasonable only as a supplemental index if admin or
  reporting workloads spend meaningful time scanning broad historical windows.
- Keep the existing B-tree indexes that support per-user and seq-based access.
- If there is balance-path latency, investigate the seq-based query path first;
  BRIN on `created_at` will not help that access pattern.

Optional DDL for reporting-heavy deployments:

```sql
CREATE INDEX CONCURRENTLY idx_credits_transactions_created_at_brin
ON credits_transactions
USING BRIN (created_at);
```

### `probe_results`

Observed characteristics:

- The table shape is append-oriented, but the important query pattern is
  per-probe history ordered by `executed_at DESC`.
- That pattern remains a better fit for composite B-tree coverage than BRIN.

Recommendation:

- Do not add a BRIN index to `probe_results` at this stage.
- If probe history queries need improvement, prefer a composite B-tree such as
  `(probe_id, executed_at DESC)` over BRIN.

### Tables that should stay off the BRIN list

The investigation did not change the earlier conclusion for these tables:

- `webhook_deliveries`
- `sync_operations`
- `sync_entries`
- `batch_capacity_reservations`

These are queue, workflow, or targeted lookup tables. Their hot paths are
selective and ordering-sensitive, which is exactly where B-tree remains the
right default.

## Recommended Action

If we want one concrete BRIN change from this investigation, it should be:

```sql
CREATE INDEX CONCURRENTLY idx_http_analytics_timestamp_brin
ON http_analytics
USING BRIN (timestamp);
```

Everything else is either optional or premature:

- `credits_transactions(created_at)` is optional and only justified by
  reporting-heavy scans across large time windows.
- `probe_results(executed_at)` is not recommended.
- No existing B-tree index should be dropped solely because a BRIN is added.
