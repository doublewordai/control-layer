# DB Index Management Findings

## Scope

This document captures broader index-management conclusions from reviewing the
two Postgres-backed repos and their live schemas:

- `control-layer`
- `fusillade`

It is intentionally public-safe. It does not include environment-specific row
counts, query text, or operational measurements. It records only conclusions
that follow from the open-source schema, migration history, and representative
query shapes.

## Main Conclusion

The next phase of index work across these databases should focus more on
improving B-tree design than on adding new index types.

In practice, that means:

- more partial B-tree indexes for live or active subsets,
- tighter composite B-tree indexes that match real `WHERE` + `ORDER BY`
  patterns,
- selective use of covering indexes with `INCLUDE`,
- and careful review of overlapping index families on very large tables.

BRIN still has a place, but it is the exception rather than the main strategy.

## Recommended Index Strategy

### 1. Prefer partial B-tree indexes for live subsets

Across both repos, a large share of important reads target a subset of rows:

- `deleted_at IS NULL`
- `state = 'pending'`
- active or terminal workflow subsets
- optional foreign keys where `... IS NOT NULL`

This should remain the default pattern for operational indexes.

Why:

- smaller indexes,
- lower write amplification,
- better cache residency,
- and a closer match to real workload shape.

This is especially relevant in:

- `fusillade.files`
- `fusillade.batches`
- `fusillade.requests`
- `control-layer` workflow tables such as webhooks and sync tables

### 2. Match composite B-tree indexes to query shape, not just columns

The biggest practical wins are still coming from composite B-tree indexes that
mirror application access patterns:

- equality filters first,
- then range condition,
- then sort columns,
- then a stable tiebreaker such as `id` for cursor pagination.

This is more important than adding unusual index types.

Examples of the kinds of patterns that matter here:

- per-user recent history
- newest-first live listing
- active-first administrative listing
- queue claims on state/model/not-before subsets
- per-batch request enumeration

## High-Confidence Follow-Up Changes

### `control-layer`

Keep the BRIN recommendation from the separate BRIN document:

- add `http_analytics(timestamp)` BRIN

Outside BRIN, the biggest ongoing opportunities are likely to be:

- more targeted partial indexes for live subsets,
- and review of overlapping large analytics and ledger indexes.

### `fusillade`

The highest-confidence non-BRIN change remains:

```sql
CREATE INDEX CONCURRENTLY idx_files_live_created_at_id
ON fusillade.files (created_at DESC, id DESC)
WHERE deleted_at IS NULL;
```

Why this matters:

- `files` has a live-file newest-first listing path.
- That query shape wants an ordered partial B-tree.
- BRIN does not help the top-N ordering path.

## Overlapping Index Families Worth Reviewing

These are not automatic drop recommendations. They are review targets where one
index may be partially or fully subsuming another, and where large tables make
that worth auditing carefully.

### `control-layer.credits_transactions`

Review the overlap between:

- `(user_id, created_at DESC)`
- `(user_id, created_at DESC, id DESC)`

General finding:

- On large append-heavy tables, keeping both a shorter and a longer variant of
  nearly the same recency index may not be necessary.
- Before dropping anything, confirm which queries depend on the extra `id`
  ordering and whether the longer index satisfies all important uses of the
  shorter one.

### `control-layer.http_analytics`

Review the family of user-scoped usage indexes together rather than one by one.

General finding:

- There are multiple large user/time analytics indexes with similar leading key
  structure.
- The right approach is not “drop the biggest one”, but “audit which query
  classes each one still serves and whether any can be consolidated”.

### `fusillade.requests`

Review the overlap between:

- `(batch_id, state)`
- `(batch_id, state) INCLUDE (...)`

General finding:

- When two indexes share the same leading key columns and one is effectively a
  covering version of the other, the smaller one may no longer be needed.
- This should only be changed after verifying the exact query set and write
  costs, because these indexes sit on a critical workflow table.

### `fusillade.batches`

Review point-lookup indexes on `output_file_id` and `error_file_id` together
with the unique constraints on those same columns.

General finding:

- When a unique B-tree already exists on a nullable column, an additional
  partial non-null B-tree can be redundant for some workloads.
- Whether it is safe to remove depends on the exact lookup pattern and planner
  preference.

### `fusillade.request_templates`

Review the family of `file_id`-prefixed indexes together:

- `(file_id)`
- `(file_id, line_number)`
- `(file_id, model)`

General finding:

- Large template tables often accumulate several `file_id`-prefixed indexes
  that are each justified individually but expensive together.
- This family should be audited as a whole if write cost or storage becomes a
  concern.

## When To Use Other Index Types

### Trigram GIN

Use trigram GIN only if substring search becomes a real latency problem.

Potential candidates:

- file name search
- `custom_id` substring search
- other `%term%` text filters

Why not by default:

- higher write cost,
- larger indexes,
- and many administrative search fields are not hot enough to justify it.

### JSONB GIN

Use JSONB GIN sparingly.

Rule of thumb:

- if a JSON key becomes operationally important, first ask whether it should be
  a typed column,
- otherwise use a targeted expression index or JSONB GIN only when the query
  pattern clearly demands it.

In these repos, JSONB is present, but there is not yet a strong general case
for broad JSONB GIN rollout.

### Expression Indexes

These are worth considering where queries normalize or transform values before
filtering.

Typical triggers:

- `LOWER(column)` search
- derived timestamp/date buckets
- extracted JSON keys used as first-class filters

This is likely a better fit than new index types when a plain B-tree is being
missed only because the query applies a function first.

## Operational Guidance

### Audit by index family, not one index at a time

Large tables in both repos now have groups of related indexes. The right review
question is usually:

- “Which query family does this group serve?”

not:

- “Is this single index big?”

That is particularly true for:

- analytics tables,
- request tables,
- and template tables.

### Be conservative on queue and scheduler tables

For workflow-critical tables such as `fusillade.requests`, index changes should
be treated as correctness-adjacent performance work.

That means:

- do not replace claim-path indexes casually,
- do not remove state-oriented indexes without plan verification,
- and prefer additive changes over replacement unless the overlap is obvious.

### Add indexes only with a named workload in mind

Every new index should be justified by a concrete workload class:

- newest-first listing,
- broad historical reporting,
- pending queue claim,
- per-user ledger history,
- etc.

If an index does not have a clearly named workload, it is usually not worth
keeping long-term.

## Final Recommendations

### Add now

1. `control-layer`

```sql
CREATE INDEX CONCURRENTLY idx_http_analytics_timestamp_brin
ON http_analytics
USING BRIN (timestamp);
```

2. `fusillade`

```sql
CREATE INDEX CONCURRENTLY idx_files_live_created_at_id
ON fusillade.files (created_at DESC, id DESC)
WHERE deleted_at IS NULL;
```

### Consider next

1. Review overlap within the `credits_transactions` recency index family.
2. Review overlap within the `http_analytics` user-usage index family.
3. Review overlap within the `fusillade.requests` batch/state index family.
4. Review overlap within the `fusillade.batches` file-reference index family.
5. Review overlap within the `fusillade.request_templates` `file_id`-prefixed
   index family.

### Use only if workload proves it

- `credits_transactions(created_at)` BRIN
- `fusillade.requests(created_at)` BRIN
- trigram GIN for substring search
- JSONB GIN for repeated JSON containment filters

## Bottom Line

The database-management theme across both repos is:

- keep using B-tree as the default,
- make those B-trees more selective and more workload-shaped,
- use BRIN only for truly large historical scan tables,
- and periodically audit large overlapping index families instead of letting
  them accumulate indefinitely.
