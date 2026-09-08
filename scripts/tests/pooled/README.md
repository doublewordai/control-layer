# Transaction-pooled application tests

The `dwctl / pooled database e2e` CI job runs the built application image against
PostgreSQL and PgBouncer. Merge groups, fork PRs, and Dependabot use a locally
built binary when image publication is skipped. The required `workspace / rust gate` depends on this job.
It runs on ordinary pull requests and merge-group checks, with the same
release-only skip policy as the rest of CI.

PgBouncer uses two backends per role, round-robin reuse, and `DISCARD ALL` after
every transaction. The test first verifies backend switching and loss of session
GUCs, LISTEN registrations, and advisory locks. This makes accidentally moving
session-dependent operations onto query pools fail deterministically, instead of
passing because a client happens to reuse the same PostgreSQL backend.

The application checks cover:

- Startup migrations in separate main, fusillade, and outlet schemas.
- A leader lock that remains held during query traffic and is released at shutdown.
- Newly created models/keys, revocation, and probe scheduling through LISTEN,
  with timeouts much shorter than fallback polling intervals.
- Concurrent inference, outlet logging, underway batch validation, fusillade
  processing, and output-file retrieval.
- Real retirement of an empty expired response partition after restart, including
  detach/drop through the direct maintenance connection. A database event trigger
  on the disposable partition verifies nonzero session timeout bounds during DDL.
- Completed batch and output persistence, migration replay, and graceful shutdown.

Only the external model provider is replaced by a deterministic local HTTP server.
Primary and replica query pools target the same PostgreSQL instance; this job does
not simulate replication lag. It does not run browser, external provider, email,
or webhook delivery tests. Component roles are non-superusers and inherit the
migration/table-owner role. Diagnostic application and PgBouncer logs are uploaded
on success or failure. Temporary databases, roles, and processes are cleaned up.

## Run locally

Requires Python 3.12+, PostgreSQL 16+ running locally with an administrative test
user, and PgBouncer 1.21+ on PATH. Use a disposable PostgreSQL instance: the harness
creates a uniquely named database and roles and removes only those resources.
Do not run PgBouncer as root.

```sh
python3 -m venv /tmp/pooled-e2e-venv
/tmp/pooled-e2e-venv/bin/pip install -r scripts/tests/pooled/requirements.txt
cargo build -p dwctl
POOLED_TEST_DATABASE_URL=postgres://postgres:password@127.0.0.1:5432/postgres \
  /tmp/pooled-e2e-venv/bin/python scripts/tests/pooled/e2e.py \
  --binary target/debug/dwctl --artifacts /tmp/pooled-e2e-results
```

On Linux, `--image IMAGE` runs the same checks against an application image using
Docker host networking. CI uses this mode to test the actual release artifact.
The host-networked image and mock provider communicate over loopback.

To check that the test detects a listener-routing regression, temporarily replace
`DynPools::new(pools.main.direct.clone())` with
`DynPools::new(pools.main.pooled.clone())` in the application's listener provider,
build a separate binary, and run the harness against it. The notification test
must fail before its 15-second deadline; restore the production code afterwards.
