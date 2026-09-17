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
command). It deliberately contains no schema knowledge of its own: every
statement that changes the database lives in a migration file.

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
   validation in a following migration.
2. SQLx records the migration only after the statement returns. An
   interrupted build (cancelled backend, killed pod, "deadlock detected")
   leaves an INVALID index and no row. On the next run `IF NOT EXISTS` sees
   the invalid index, skips the build, and the migration is recorded as
   applied — the 11.9.1 incident. `IF NOT EXISTS` is therefore never proof
   that the index is usable.
3. **Make the sequence recoverable in SQL.** Ship three files, and leave
   numbering room between them (timestamps ten seconds apart, never
   adjacent integers) so a repair can be slotted in later if ever needed:

   | file                              | contents                                             |
   |-----------------------------------|------------------------------------------------------|
   | `…10_add_<index>.up.sql`          | `-- no-transaction` + `CREATE INDEX CONCURRENTLY IF NOT EXISTS …` |
   | `…20_reindex_<index>.up.sql`      | `-- no-transaction` + `REINDEX INDEX CONCURRENTLY <index>;`       |
   | `…30_validate_<index>.up.sql`     | transactional `DO $$ … $$` checking definition, `indisvalid`, `indisready`; `COMMENT ON INDEX` |

   The reindex step is what makes an interrupted build self-healing: on a
   clean build it is one extra concurrent pass (bounded, no write lock), on
   an interrupted one it turns the INVALID index into a valid one, and if
   *it* is interrupted the next run repeats it. Neither step is recorded
   until it succeeds, so a retry always resumes at the right place. The
   validation step then fails only for a genuinely wrong definition, which
   is an operator decision (rename or drop by hand), never something a
   migration guesses at.

   If `REINDEX INDEX CONCURRENTLY` itself is interrupted it can leave an
   invalid `<index>_ccnew` behind; the next `REINDEX` run tolerates it, and
   `DROP INDEX CONCURRENTLY IF EXISTS <index>_ccnew` in its own
   `-- no-transaction` file cleans it up.

Reads while the build runs are unaffected. The previous release keeps using
whatever index it used before; do not drop that one in the same release.

### Partitioned tables

`CREATE INDEX CONCURRENTLY` is not supported on a partitioned parent. The
procedure is: create the parent index `ON ONLY` (metadata only), build one
child `CONCURRENTLY` per leaf partition, `ATTACH PARTITION` each child; the
parent becomes valid once every partition has an attached valid child. That
needs a statement per partition, so it cannot be a migration on a populated
database. Keep the existing pattern: the migration builds directly only when
every partition is empty and otherwise validates that
`scripts/prepare_retained_*_index.sql` was run beforehand; document the
prebuild in the release notes for that version. Partitions created later
inherit the parent index automatically.

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

* `cargo test -p dwctl --lib migrations::` passes.
* New `CONCURRENTLY` index → build, reindex and validate files with
  numbering room between them.
* The previous release can serve against the migrated schema (additive, or
  the expand half of an expand/contract).
* `.github/scripts/test-fusillade-migration-checksums.py` (run by `just lint
  rust`) still passes: released files untouched.
* Long lock? `SET LOCAL lock_timeout` at the top.
