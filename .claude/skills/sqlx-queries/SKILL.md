---
name: sqlx-queries
description: Use when writing or reviewing control-layer SQLx queries, repository methods, QueryBuilder SQL, SELECT or RETURNING projections, pool routing, or cached-plan result-type errors.
---

# SQLx queries in control-layer

Keep database access in `dwctl/src/db/handlers/` repositories. Follow the nearby
repository's connection/transaction and error conventions; handlers remain
generic over `PoolProvider`. Paths below are repository-root-relative.

## Write queries with stable results

- Name result columns explicitly in `SELECT` and in INSERT/UPDATE/DELETE
  `RETURNING`. Do not use table-result `SELECT *`, `alias.*`, or `RETURNING *`,
  even when the Rust struct currently matches the whole table.
- Match the intended Rust row fields, types, nullability, and SQLx aliases.
  When replacing a wildcard, preserve the existing result descriptor; review
  regenerated `.sqlx` metadata for unintended changes.
- Prefer `query!` / `query_as!` for static SQL. Bind values with parameters;
  use `.bind()` for runtime queries and `QueryBuilder::push_bind()` for dynamic
  values. Only append trusted SQL fragments or allowlisted identifiers with
  `push()`; never interpolate request input into SQL.
- Runtime `query_as` and `QueryBuilder` SQL needs execution tests: SQLx prepare
  does not compile-check those query strings.

Example from the groups repository, inside its transaction:

```rust
let group = sqlx::query_as!(
    Group,
    r#"
    INSERT INTO groups (name, description, created_by, source)
    VALUES ($1, $2, $3, 'native')
    RETURNING id, name, description, created_by, created_at, updated_at, source
    "#,
    request.name,
    request.description,
    request.created_by
)
.fetch_one(&mut *self.db)
.await?;
```

`COUNT(*)` does not expand a row. Fixed CTE/typed UNNEST projections need review
of the complete query before an exemption: the final client-visible result must
remain stable. Do not exempt a table wildcard merely to pass the guard.

## Choose the connection by semantics

| Operation | Connection |
| --- | --- |
| Mutation or atomic multi-step operation | Primary `.write()` transaction; scope repository borrows, then commit |
| Read immediately after a write, including POST then GET across endpoints | Primary `.write()` |
| Read that tolerates replication lag | `.read()` |
| LISTEN, session advisory locks, migrations/maintenance | Existing dedicated direct connection/provider, never a transaction-pooled query connection |

Preserve the existing schema-routing helpers, including Fusillade's
transaction-local schema selection where configured. Session `SET` state cannot
be assumed to survive transaction pooling.

## Migration compatibility

SQLx compile checks validate one schema snapshot. PgBouncer can retain prepared
statements after all application clients restart. Adding a column can therefore
break wildcard results with `cached plan must not change result type`.

Explicit columns protect additive changes, not in-place changes to selected
column types. Use expand → backfill → switch readers/writers → contract after
old clients drain. Do not blindly replay writes to recover from cached-plan
errors or treat SQLx `persistent(false)` as invalidation of an existing statement.

See [the migration explainer](../../../docs/schema-safe-migrations.md) and use
[sqlx-index-migrations](../sqlx-index-migrations/SKILL.md) for index DDL.

## Validate changes

Use `#[sqlx::test]` and repository/API tests, including relevant dynamic filters,
empty results, nulls, and read-after-write workflows. Refresh `.sqlx` metadata with
`cargo sqlx prepare --workspace` against correctly migrated local databases;
preserve per-crate database configuration. Run `just lint rust` and
`just test rust` before pushing Rust changes.

Lint runs `scripts/check_query_projections.py`; reviewed exceptions live in
`.github/fixtures/stable-query-projections.json`. This source guard is not a SQL
parser. For migration-sensitive queries, extend and run both shared/scoped
[pooled E2Es](../../../scripts/tests/pooled/README.md): warm queries, apply DDL,
then exercise existing and replacement clients while PgBouncer stays alive.
