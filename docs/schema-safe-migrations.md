# Database migrations with rolling deployments

An application restart does not necessarily create new PostgreSQL sessions.
Transaction poolers can retain prepared statements and share them between old
and new application clients. Migrations must support both application versions
and the prepared statements already present on those server connections.
A separate pre-rollout migration runner can control DDL ordering, but a successful
migration ledger or startup compatibility check does not prove that retained
prepared result descriptors are compatible. These query and regression safeguards
are needed with either startup migrations or a separate migration runner.

## Keep result shapes stable

Use explicit column lists for query results, including `RETURNING`. Avoid
`SELECT *`, `alias.*`, and `RETURNING *` over database tables. SQLx compile-time
checking validates against one schema snapshot; it does not prove that an
already-prepared statement survives a later migration. Adding an unrelated column
changes a wildcard statement's result descriptor, and PostgreSQL can reject the
next execution with SQLSTATE `0A000`, `cached plan must not change result type`.

A wildcard over an explicit, fixed CTE or a typed `UNNEST` is different: its
output does not automatically grow when a base table changes. Review the entire
projection chain before exempting it. `COUNT(*)` is an aggregate and does not
expand the table's columns.

## Expand, deploy, then contract

1. Add a nullable column or a compatible default. Existing writers must continue
   to succeed without supplying the new field.
2. Deploy code that understands both states. If replacing a column, use separate
   old and new columns, dual-write where necessary, and backfill in bounded work.
3. Verify the backfill and switch readers. Keep old columns while previous
   application generations, workers, and in-flight transactions may use them.
4. Remove obsolete columns or constraints in a later migration only after the
   old clients have drained and no supported application version depends on them.

Do not change the type of a selected column in place during a rolling deployment.
Explicit projections still depend on that column's type. Use a replacement column
and the expand/contract sequence. Renames, drops, NOT NULL changes, and constraint
changes also need old-reader and old-writer compatibility review.

Use the direct migration connection for DDL. Preserve applied migration bytes.
Follow the repository's concurrent-index migration guidance for index changes;
result-shape compatibility does not make a locking table rewrite safe.

## Regression checks

The required pooled database E2E jobs cover both shared and separate schema
roles. Their first phase deliberately clears session state to detect LISTEN,
advisory-lock, and schema routing regressions. Their schema-change phase instead
retains prepared statements and keeps PgBouncer running while the application is
restarted.

The fixture must reproduce wildcard SELECT and RETURNING failures after ADD
COLUMN, including a fresh client's reuse of an old backend plan. It must also
show explicit columns surviving additive DDL and failing on incompatible column
type changes. The actual models API is warmed, migrated, queried, restarted,
and queried again without restarting its pooler. A fixture that always clears
statements is insufficient for testing schema compatibility.

## Query audit guard

`just lint rust` runs `scripts/check_query_projections.py` and its regression
tests. The source guard rejects wildcard SELECT and RETURNING projections in
application/database code. Reviewed exceptions live in
`.github/fixtures/stable-query-projections.json` with an explanation and exact
source occurrence counts, including deliberate negative-test queries. A changed or removed exception requires updating the
audit; do not add a table-result wildcard just to silence the guard.

The guard is deliberately conservative and is not a full Rust or SQL parser.
Dynamically assembled projections and result-type changes still require review
and execution through the live-pooler migration tests. Existing archive
INSERT/SELECT projections are separately governed by archive schema-parity tests.

## Diagnostic metric

`dwctl_db_cached_plan_errors_total` counts result-shape invalidation errors passing
through the shared database error conversion. It is registered at zero when the
Prometheus recorder is installed, so the first observed error can contribute to a
counter increase. Other SQLSTATE `0A000` errors are excluded. The metric contains
no query text, resource identifiers, or customer data. Direct database calls that
bypass this conversion require their own error monitoring.

## Handling an unexpected cached-plan error

Identify the query and schema change. Prefer changing an unstable wildcard query
to an explicit projection; the new SQL text receives a distinct prepared
statement. Do not blindly retry the same failing query or replay writes. A failed
statement inside a transaction requires rollback before recovery, and closing an
application connection alone may not remove the backend's prepared statement.
In SQLx 0.8, `persistent(false)` still reuses an existing cached statement for the
same SQL text; it does not by itself invalidate the old plan. A regression test
covers this behavior. Automatic retry is therefore deliberately not installed at
the shared error boundary: it cannot know whether the failed operation is a safe
read, whether its transaction has been rolled back, or whether the next execution
would actually prepare a compatible statement.

PgBouncer documents RECONNECT after incompatible DDL, but managed services may
not expose its admin console. Establish the provider-supported procedure and
its impact before using it. Do not make restarts or scaling the application to
zero the normal migration procedure.
