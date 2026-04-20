# BRIN Index Recommendations

## Scope

This document captures the final, public-safe conclusions from BRIN index
investigations across the two Postgres-backed repos:

- `control-layer`
- `fusillade`

It intentionally avoids environment-specific operational details. It records
only conclusions that are justified by the public schema, migrations, and
application query shapes.

## What BRIN Is Good For

BRIN is appropriate when a table is:

- large,
- append-heavy,
- physically correlated with a time-like column,
- and queried by broad range scans rather than point lookups or ordered queue
  access.

BRIN is not a replacement for B-tree indexes that support:

- newest-first pagination,
- selective foreign-key lookups,
- queue claiming,
- active-first ordering,
- or other latency-sensitive workflow queries.

## Recommended Changes

### `control-layer`

#### Add: `http_analytics(timestamp)` BRIN

`http_analytics` is the strongest BRIN candidate in `control-layer`.

Why:

- It is an append-heavy analytics table.
- Its hot analytical queries are broad time-window scans over `timestamp`.
- The existing B-tree on `timestamp` still makes sense for ordered pagination
  and recent-request lookup, so BRIN should be supplemental rather than a
  replacement.

Recommended DDL:

```sql
CREATE INDEX CONCURRENTLY idx_http_analytics_timestamp_brin
ON http_analytics
USING BRIN (timestamp);
```

Keep:

- `idx_analytics_timestamp`
- `(model, timestamp)` and other selective analytics B-trees

Do not drop existing B-tree indexes just because the BRIN is added.

#### Optional only: `credits_transactions(created_at)` BRIN

`credits_transactions` is technically BRIN-friendly, but only for broad
historical reporting.

Why it is not a default change:

- Important reads are still per-user and seq-based.
- Balance and checkpoint logic depend on B-tree-shaped access patterns.

Optional DDL for reporting-heavy deployments:

```sql
CREATE INDEX CONCURRENTLY idx_credits_transactions_created_at_brin
ON credits_transactions
USING BRIN (created_at);
```

Default recommendation:

- Do not add this index unless broad historical transaction scans are a real
  problem.

#### Do not add BRIN to these `control-layer` tables

- `probe_results`
- `webhook_deliveries`
- `sync_operations`
- `sync_entries`
- `batch_capacity_reservations`

Reason:

- Their important access patterns are selective, ordering-sensitive, or queue
  shaped.
- Those paths are still better served by B-tree indexes.

For `probe_results`, if performance work is needed, prefer a composite B-tree
such as `(probe_id, executed_at DESC)` over BRIN.

### `fusillade`

#### Optional only: `requests(created_at)` BRIN

`fusillade.requests` is the only credible BRIN candidate in the Fusillade
schema.

Why:

- It is one of the largest tables in the system.
- Historical/admin request scans use `created_at` windows.
- The table is large enough that a compact pruning index can make sense for
  broad time-range reads.

Why it is only supplemental:

- Core queueing and claim paths are not time-range scans.
- The important scheduler indexes are driven by `state`, `model`, `batch_id`,
  `template_id`, and `not_before`.
- Ordered request listing still benefits from the existing B-tree on
  `created_at`.

Recommended DDL when historical scans justify it:

```sql
CREATE INDEX CONCURRENTLY idx_requests_created_at_brin
ON fusillade.requests
USING BRIN (created_at);
```

Default recommendation:

- Do not replace the current `created_at` B-tree.
- Do not modify queue-oriented request indexes as part of BRIN work.

#### Do not add BRIN to these `fusillade` tables

- `request_templates`
- `batches`
- `files`
- `daemons`

Reason:

- `request_templates` is large, but its real access paths are keyed by
  `file_id`, `line_number`, `custom_id`, and similar selective lookups.
- `batches` and `files` are dominated by newest-first listing and workflow
  filters.
- `daemons` is too small to justify BRIN.

## More Important Non-BRIN Fix In `fusillade`

The best index improvement identified in Fusillade was not BRIN.

### Add: `files(created_at DESC, id DESC) WHERE deleted_at IS NULL`

The default `files` listing path is a live-file newest-first read. That wants a
partial B-tree, not BRIN.

Recommended DDL:

```sql
CREATE INDEX CONCURRENTLY idx_files_live_created_at_id
ON fusillade.files (created_at DESC, id DESC)
WHERE deleted_at IS NULL;
```

This is a higher-priority change than adding BRIN anywhere in Fusillade except
for a proven historical-scan problem on `requests`.

## Final Action List

### High confidence

1. In `control-layer`, add:

```sql
CREATE INDEX CONCURRENTLY idx_http_analytics_timestamp_brin
ON http_analytics
USING BRIN (timestamp);
```

2. In `fusillade`, add:

```sql
CREATE INDEX CONCURRENTLY idx_files_live_created_at_id
ON fusillade.files (created_at DESC, id DESC)
WHERE deleted_at IS NULL;
```

### Optional, workload-dependent

1. In `control-layer`, consider:

```sql
CREATE INDEX CONCURRENTLY idx_credits_transactions_created_at_brin
ON credits_transactions
USING BRIN (created_at);
```

Only if broad historical transaction reporting is an actual bottleneck.

2. In `fusillade`, consider:

```sql
CREATE INDEX CONCURRENTLY idx_requests_created_at_brin
ON fusillade.requests
USING BRIN (created_at);
```

Only if broad historical request scans are an actual bottleneck.

### Do not pursue

- BRIN on `control-layer.probe_results`
- BRIN on `control-layer` queue/workflow tables
- BRIN on `fusillade.request_templates`
- BRIN on `fusillade.batches`
- BRIN on `fusillade.files`
- BRIN on `fusillade.daemons`

These remain B-tree problems, not BRIN problems.
