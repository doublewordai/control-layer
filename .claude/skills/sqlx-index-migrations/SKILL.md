---
name: sqlx-index-migrations
description: Create or review control-layer index migrations using SQLx, including concurrent DDL, prebuilt indexes, and follow-up definition validation and comments.
---

# SQLx index migrations

Use SQLx migrations as the single source of index DDL. Predeploy builds execute
those same files; do not duplicate the DDL in standalone scripts. Existing
predeploy scripts are historical examples, not the preferred pattern.

## File structure

- Start each concurrent create/drop file with exactly `-- no-transaction`.
  The older `-- sqlx:no-transaction` spelling found in this repo is not recognized
  by SQLx 0.8.
- Include **one executable statement per concurrent file**. Put `COMMENT ON`,
  validation, and other operations elsewhere: disabling SQLx's transaction does
  not prevent a multi-statement batch from creating an implicit transaction.
- Follow a create with a normal transactional migration that validates the index,
  then adds `COMMENT ON INDEX` explaining its query/access pattern and issue.
- Reject missing, invalid, not-ready, and wrong-definition indexes. Check the
  target table/schema, method, uniqueness, keys and ordering, included columns,
  and predicate as applicable. `IF NOT EXISTS` only checks the name and can skip
  an unusable remnant of an interrupted build.

Use `*add_batch_owner_page_index*` and `*validate_batch_owner_page_index*` in
`fusillade-arsenal/migrations/` as examples. The wrong-order regression is in
`fusillade-arsenal/src/postgres/batch_list.rs`. Paths are repo-root-relative.

## Repository integration

- Migration directories: `dwctl/migrations/` and `fusillade-arsenal/migrations/`.
  Respect Fusillade's dedicated-database and shared-`fusillade`-schema modes;
  resolve objects using the runner's search path rather than assuming `public`.
- Preserve applied migration bytes. For new Fusillade up/down files, add SHA-384
  entries to `.github/fixtures/fusillade-migration-sha384.txt`; never rewrite old
  entries to accommodate changes. Check with
  `python3 .github/scripts/test-fusillade-migration-checksums.py`.
- Validate through the actual SQLx runner on both fresh and correctly prebuilt
  states, plus rejection of invalid and wrong-definition indexes. Executing
  the files only through psql misses SQLx transaction and bookkeeping behavior.

For populated deployments, prebuild the concurrent file separately on a direct
connection in autocommit mode with the intended database/search path. Run the
validation/comment file, then ANALYZE the affected table before deploying dependent
queries. Startup SQLx migrations skip the existing build, validate it again, and
record it normally; do not manually mark the prebuild as applied. If validation
fails, inspect and repair the specific index before proceeding.
