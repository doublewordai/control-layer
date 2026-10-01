---
name: rust-test-efficiency
description: Write, review, or profile Rust tests in control-layer while avoiding repeated database and application setup. Use for Rust test changes and slow-suite investigations, not production query optimization.
---

# Efficient Rust tests

Prefer a plain `#[test]` for parsing, validation, permission calculations, and
other pure logic. Use `#[tokio::test]` when asynchronous code needs no database.
Use a database test when PostgreSQL behavior, repository queries, transactions,
schema constraints, or the real HTTP/authentication path are part of the claim.

## Isolated database tests

In dwctl unit tests use `#[dwctl_test_macros::test]` with an `async fn` taking
one `PgPool`. The helper in `dwctl/src/test/template.rs` clones a migrated
database; every test still owns its data and can run concurrently. SQLx-style
`fixtures(path = "fixtures", scripts("name"))` are applied to the clone in order.
Do not add shared mutable rows to the template or cache an application across
unrelated tests.

Keep `#[sqlx::test(migrations = false)]` for migration/startup tests that must
begin empty or apply only a migration prefix. Those must never use a fully
migrated template. Crates outside dwctl and external integration tests currently
retain SQLx's harness.

Also retain plain `#[sqlx::test]` when the main schema should exist but component
schemas/indexes should not: disabled logging, dedicated component databases,
and Underway extension installation tests depend on that initial state.

Use repository helpers directly unless routing, middleware, or application
startup is what the test exercises. Starting the complete application to test a
pure predicate adds work without adding useful coverage.

## Permission cases

For independent role × endpoint cases, create one isolated database and app,
then loop over named cases with fresh users/resources as necessary. Preserve
every positive and negative case, ownership boundary, response assertion, and
additive-role combination from the previous tests. Include the role and case in
assertion messages. Do not combine stateful workflows whose order can hide a
failure; a shared database across the whole suite is not a substitute for
isolation.

## Waiting and measurement

Poll for the actual asynchronous condition with a deadline. Assert the initial
state and the eventual state. Avoid fixed sleeps, and keep real-clock waits
only when elapsed time is the behavior under test. Do not use retries to hide
test failures.

`just test rust` uses nextest and then runs doctests separately. Nextest filters
apply to unit/integration tests; doctests still run for the selected packages.
Slow tests are reported every ten seconds. For per-test durations, inspect
`target/nextest/default/junit.xml` after a run. For one slow dwctl test:

```bash
DW_TEST_TIMINGS=1 cargo nextest run -p dwctl -E 'test(test_name)' \
  --profile timings --success-output final
```

The helper reports clone/fixture setup, body, and total time. Body time includes
application startup; the `create_test_app_with_config` helper reports that
separately when timing is enabled. Use `-j 1` for isolated measurements and the
same concurrency for before/after suite comparisons. Do not compare a cold
build against a warm run. Record compile/link time separately with
`cargo nextest run --workspace --all-features --no-run`.

For a controlled comparison of migration setup versus cloning, repeat the
same test with `DW_TEST_FRESH_DATABASES=1 DW_TEST_TIMINGS=1`. This builds
the same schemas without using the cache. Templates are named from migration
checksums and `Cargo.lock`; migration changes select a new template automatically.
Changing the template construction procedure also requires changing its format
version in the helper.

## Review checklist

- Is a database/full app needed for the behavior being asserted?
- Does each test retain its own database and deterministic fixtures?
- Could independent permission cases reuse one app without losing assertions?
- Are waits bounded by the observed condition rather than a fixed delay?
- Have slow tests been measured, with build time separated from execution?
- Do migration tests still start with the schema state they are meant to test?
