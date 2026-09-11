# Writing schema migrations

How migrations run, and how to write one that survives a rolling deployment.

## How migrations run

There are three SQLx migrators plus the `underway` task queue:

| target      | files                          | schema                                   |
|-------------|--------------------------------|------------------------------------------|
| `main`      | `dwctl/migrations`             | main database, default schema            |
| `fusillade` | `fusillade-arsenal/migrations` | `database.fusillade` (schema or database)|
| `underway`  | the `underway` crate           | `underway` schema of the main database   |
| `outlet`    | the `outlet-postgres` crate    | `database.outlet`, when logging is on    |

`dwctl migrate` applies all of them in that order and exits; `dwctl migrate
--check` verifies without DDL. The serving process does one or the other at
startup depending on `migrations.mode` in `config.yaml`:

* `run` (default): apply pending migrations, then serve. Local development,
  tests, single-instance installs.
* `check`: never execute DDL; verify every migration this binary ships is
  recorded with a matching checksum, refuse to start otherwise, and accept a
  database that is *ahead*. Deployments that run the migration Job use this.

In production the Helm chart runs `dwctl migrate` as an Argo CD PreSync hook
Job with the release image before any pod of that release is created. A failed
Job blocks the rollout; the previous release's pods keep serving. That has a
consequence for every migration you write:

> **The previous release's pods keep running against the migrated schema until
> the rollout completes.** Every migration must be additive with respect to
> the previous release, or expand/contract across two releases.

The implementation is `dwctl/src/migrations.rs` (runner, compatibility check,
command) and `fusillade-arsenal/src/managed_index.rs` (index recovery).

## Transactional migrations (the default)

A migration file runs inside one transaction, and SQLx records it in
`_sqlx_migrations` in the same transaction. Either everything in the file
happened, or nothing did. Prefer this whenever Postgres allows it:

* `CREATE TABLE`, `ALTER TABLE ... ADD COLUMN` (nullable, or with a constant
  default — that is a metadata-only change), `CREATE INDEX` on an empty or
  small table, `CREATE FUNCTION`, data backfills bounded in size.
* Constraints on populated tables: `ADD CONSTRAINT ... NOT VALID` in one
  statement, `VALIDATE CONSTRAINT` in a later one, so the validation scan does
  not hold an exclusive lock. Set `SET LOCAL lock_timeout = '5s'` at the top of
  any migration that takes a table lock on a hot table, so a blocked migration
  fails fast and the Job's next attempt retries, instead of queueing every
  request behind it.

Naming: `dwctl/migrations/NNN_description.sql` (sequential), or
`fusillade-arsenal/migrations/YYYYMMDDhhmmss_description.up.sql` plus a
`.down.sql`. **Never edit a migration after it has been released.** SQLx
checksums each file; a changed checksum stops both the Job and every
`check`-mode pod (`released migration file(s) changed after they were
applied`). Ship a new migration instead.

## Concurrent index builds

`CREATE INDEX` on a populated table blocks writes for the duration of the
build, so on hot tables use `CREATE INDEX CONCURRENTLY`. That comes with
rules, because it cannot run inside a transaction:

1. The file must start with `-- no-transaction` as its **first bytes** (SQLx
   matches with `starts_with`) and contain **exactly one statement**:
   Postgres wraps a multi-statement simple query in an implicit transaction,
   which `CONCURRENTLY` rejects. Put the `COMMENT ON INDEX` and any
   validation in the following migration.
2. SQLx records the migration only after the statement returns. An
   interrupted build (cancelled backend, killed pod, "deadlock detected")
   leaves an INVALID index and no row. On the next run `IF NOT EXISTS` sees
   the invalid index, skips the build, and the migration is recorded as
   applied — the 11.9.1 incident. `IF NOT EXISTS` is therefore never proof
   that the index is usable.
3. **Register the index in `fusillade_arsenal::managed_index::managed_indexes()`**
   (or the equivalent list for the target). The migration runner inspects
   every registered index before the migrator runs: an invalid index with the
   intended definition is rebuilt with `REINDEX INDEX CONCURRENTLY`, an absent
   index whose migration is already recorded is built, and an index with a
   different definition fails the run with both definitions in the message
   and is never touched. After the migrator the runner verifies every
   registered index is valid, and `ANALYZE`s any table whose index it built.

A registry entry:

```rust
ManagedIndex {
    name: "idx_batches_owner_created_at_id",
    table: "batches",
    // Exactly what pg_get_indexdef prints after `ON <table> `; doubles as the
    // SQL used to (re)build it. `managed_index_definitions_match_catalog`
    // fails if this drifts from the migration.
    definition: "USING btree (created_by, created_at DESC, id DESC) WHERE (deleted_at IS NULL)",
    created_by_migration: 20260910010000,
    kind: IndexKind::Plain,
},
```

Get the `definition` string from a migrated database:
`SELECT pg_get_indexdef('idx_...'::regclass);` and take everything from
`USING`. The test pins it, so a wrong string fails `cargo test -p
fusillade-arsenal managed_index` rather than production.

Pair the build with a validation migration (see
`20260910010001_validate_batch_owner_page_index.up.sql`) that checks the
definition in `pg_index` and raises otherwise. With the registry in place the
validation is belt and braces: it documents the intent in SQL and makes
`sqlx migrate run` on a laptop fail the same way the Job would.

Reads while the build runs are unaffected. The previous release keeps using
whatever index it used before; do not drop that one in the same release.

### Partitioned tables

`CREATE INDEX CONCURRENTLY` is not supported on a partitioned parent. The
procedure is: create the parent index `ON ONLY` (metadata only), build one
child `CONCURRENTLY` per leaf partition, `ATTACH PARTITION` each child; the
parent becomes valid once every partition has an attached valid child. Register
the index with `IndexKind::Partitioned { child_prefix }` and the runner does
all of that before the migrator runs, so a populated database no longer needs
the `scripts/prepare_retained_*_index.sql` prebuild. Keep the migration itself
as the existing ones are: build directly only when every partition is empty,
otherwise validate that preparation happened.

Partitions created later inherit the parent index automatically; the runner's
per-partition check covers partitions created between two releases.

## Destructive and shape-changing migrations

Anything the previous release cannot run against must be split across two
releases (expand, then contract):

| change                          | release N (expand)                                                  | release N+1 (contract)                     |
|---------------------------------|---------------------------------------------------------------------|--------------------------------------------|
| drop a column                   | stop reading and writing it in code                                 | `ALTER TABLE ... DROP COLUMN`              |
| rename a column                 | add the new column, write both, backfill, read new                  | drop the old column                        |
| add `NOT NULL` to a column      | write it everywhere; `ADD CONSTRAINT ... CHECK (col IS NOT NULL) NOT VALID`, then `VALIDATE` | `SET NOT NULL` (instant with a validated check), drop the check |
| change a type                   | add a new column of the new type, dual-write, backfill              | drop the old column                        |
| drop an index                   | (nothing; the new release stops depending on it)                    | `DROP INDEX CONCURRENTLY` (`-- no-transaction`) |
| drop a table                    | stop using it                                                       | `DROP TABLE`                               |

Release N+1's migration runs while release N's pods have already been
replaced, so at that point nothing depends on the old shape. The
compatibility check enforces the *other* half: a release N pod that restarts
after release N+1's migrations ran still starts, because the database is
"ahead", so those migrations must not have removed anything release N reads.

Data backfills that touch many rows belong in batched statements (`UPDATE ...
WHERE id IN (SELECT ... LIMIT 10000)` in a loop from a background task) or, if
they must be a migration, in their own `-- no-transaction` file so a lock is
never held across the whole table.

## Checklist before opening a PR

* `cargo test -p dwctl --lib migrations::` and `cargo test -p
  fusillade-arsenal managed_index` pass.
* New `CONCURRENTLY` index → registry entry + validation migration +
  `managed_index_definitions_match_catalog` passes.
* The previous release can serve against the migrated schema (additive, or
  the expand half of an expand/contract).
* `.github/scripts/test-fusillade-migration-checksums.py` (run by `just lint
  rust`) still passes: released files untouched.
* Long lock? `SET LOCAL lock_timeout` at the top.
