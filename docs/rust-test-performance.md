# Rust test performance

`just test rust` runs nextest followed by doctests. Normal, `--watch`, and
`--coverage` runs share the same runner. Install it with
`cargo install cargo-nextest --locked`; `just check` checks this prerequisite.

Template-backed tests require PostgreSQL 15 or newer for `CREATE DATABASE ...
STRATEGY FILE_COPY`, and a test role with database-creation privileges (also
required by SQLx's database test harness). The local Docker configuration uses
PostgreSQL 17. Run these tests against a development/test server.

`just test rust -p dwctl` selects only that package, including its doctests.
Explicit `--features`/`-F` or `--no-default-features` replace the default
`--all-features`; explicit `--workspace` and `--all-features` remain supported.

## Local measurements

Measured against commit `44863350` on macOS with Rust 1.97.1, PostgreSQL 17
in a local container, and ten nextest workers. PostgreSQL used the local test
settings with durability disabled. These are single full-suite observations,
not CI results or a reproduction of the issue's original 30-minute machine.

| Measurement | Before | After |
| --- | ---: | ---: |
| Nextest unit/integration execution, excluding build | 263.15 s | 123.56 s |
| Cumulative dwctl unit-test time across workers | 2,137.46 s | 528.87 s |
| Observed compile/link with dependencies already built | 529.82 s | 156.97 s |
| dwctl unit-test executable, full debug symbols | 650 MiB | 460 MiB |
| Repeated permission-test database setup, median of three | 596.9 ms fresh | 64.0 ms cloned |
| Coalesced SSE framing test | 11.48 s | 0.019 s |

The unmodified native runner spent 309.30 seconds summed across test binaries,
excluding compilation and doctests. Nextest's 263.15-second execution was
already faster before changing database setup. The final `just test rust`
command took 229.88 seconds including a 62-second rebuild and doctests.
Build observations are incremental, dependency-warm builds, not controlled cold
build benchmarks. The final nextest configuration also runs memory-budget tests
exclusively; the baseline nextest run did not have that restriction.

Database setup was the main removable cost. In the controlled permission-test
comparison, application startup stayed similar (99.1 ms cloned versus 97.0 ms
fresh), as did the test body (137.9 ms versus 137.4 ms). Across the final suite,
1,319 template-backed test setups took 68.67 cumulative seconds, with a median
of 46.5 ms and p95 of 76.9 ms. Their bodies took 284.15 cumulative seconds.
The 550 instrumented application-helper calls took 127.39 cumulative seconds;
that time is included in test bodies, and excludes direct application builders.

## Changes and isolation

`#[dwctl_test_macros::test]` clones an immutable migrated PostgreSQL template
for each dwctl unit test, then loads that test's fixtures. Templates include the
main, Fusillade, Outlet, Underway, and Underway extension migrations. Migration
checksums and Underway's migration versions select the template, so unrelated
dependency bumps reuse it. A session advisory lock avoids duplicate builds
between sessions in one database. Builders migrate a private `dwctl_build_*`
database and rename it to the template only once sealed, so builders whose
`DATABASE_URL`s name different databases on one server cannot drop each other's
work. A failed build is recorded for the rest of the nextest run, so later tests
report it instead of each rebuilding the template. Tests verify concurrent clones
cannot observe each other's mutations, check every migration ledger, and cover
the build race and failure record.

Migration tests, component-schema precondition tests, other crates, and external
integration tests retain SQLx's existing harness. Successful clones are dropped.
Failed tests print their database name and retain it for inspection. Those
failed databases, interrupted `dwctl_build_*` databases, and obsolete
`dwctl_template_*` databases persist until removed from the local test server.
Never clean them up during a run.

`just db-prune-tests` lists them with their total size; after stopping test runs
in **all worktrees using that server**, `just db-prune-tests --yes` drops them.
Templates are rebuilt on demand. There is deliberately no automatic pruning
based on the current checkout's hash: another checkout may still be using
another hash.

CI's dwctl shards start PostgreSQL without fsync, matching `just db-start`.
Cloning with `STRATEGY FILE_COPY` requests a checkpoint per clone: with fsync
off it was clearly faster than `WAL_LOG` (160 clones from 16 workers: 4.4–7.1s
versus 9.9–11.8s), while with fsync on it was 12–23% slower.

The shared harness erases test-future types so migration and cleanup code is
compiled once instead of repeated in more than a thousand tests. Full local
debug symbols are retained. No reduced-debug profile was adopted or benchmarked;
the existing CI profile already disables debug information.

Four transaction permission tests now use one application with independent
users and resources per role, preserving their positive and negative assertions.
The SSE test uses three individually valid events whose combined size exceeds
the buffer limit, preserving the framing boundary without 10,000 repetitions.
The net suite count changed from 3,808 to 3,806: three fewer permission tests
and one new clone-isolation test.

## Validation and remaining failures

For the initial implementation, `just lint rust` passed, including formatting,
Clippy, SQLx preparation, repository checks, and seven runner tests. Its full run passed 3,802 tests,
failed four, and skipped the same four ignored tests. All doctests passed
(17 passed, 27 ignored). The following failures also occurred in the saved,
unmodified baseline binaries:

- `pricing::class_tests::customer_display_sort_and_usage_have_explicitly_different_class_rules`
- `pricing::class_tests::customer_quotes_hide_classes_and_ownerless_estimates_use_general_prices`
- `request_logging::batcher::integration_tests::organization_class_charges_fold_into_the_owning_keys_cap`
- `memory_budget::chat_completion_request_body`

No new failures remain, but the suite is not green. These assertions were not
weakened to produce a passing performance result.

Review fixes were validated with 23 focused Rust tests and 11 runner checks,
all passing, plus a passing `just lint rust`. The subsequent full local run
passed 3,803 tests and all doctests, with the same four baseline failures and
four ignored tests. The added test covers fixture-failure diagnostics and pool
cleanup. The initial PR's CI passed all four locally failing tests.

## Reproduce and inspect

```bash
# Separate compilation from execution.
cargo nextest run --workspace --all-features --no-run
DW_TEST_TIMINGS=1 just test rust --no-fail-fast --profile timings -j 10
python3 scripts/rust-test-timings.py target/nextest/timings/junit.xml

# Compare identical schemas with and without the template cache.
DW_TEST_TIMINGS=1 cargo nextest run -p dwctl -j 1 \
  -E 'test(test_create_transaction_permission_matrix)' --success-output final
DW_TEST_FRESH_DATABASES=1 DW_TEST_TIMINGS=1 cargo nextest run -p dwctl -j 1 \
  -E 'test(test_create_transaction_permission_matrix)' --success-output final
```

Nextest reports tests still running every ten seconds without terminating them.
The `timings` profile also retains successful test output in JUnit. Body timing
includes application startup; setup includes cloning and fixtures. The first
run after a template-key change also includes template construction.

CI logs dwctl compile/link and execution durations separately and uploads
per-shard JUnit reports. Pass multiple reports to `rust-test-timings.py` to
summarize shards. The initial [PR CI run](https://github.com/doublewordai/control-layer/actions/runs/36926465776)
passed, including all four locally failing tests. Compared with the
[preceding merged PR](https://github.com/doublewordai/control-layer/actions/runs/36882204078),
dwctl shard execution fell from 39–58 seconds to 26–33 seconds. Complete shard
jobs took 3m01s–3m06s instead of 5m57s–6m35s. Both used Rust 1.99 and the CI
profile, but only the newer run restored the Rust cache, so the observed build
reduction (4m00s–4m23s to 1m23s–1m24s) is not a controlled comparison. These
compile times come from Cargo's own duration, excluding coverage setup.

The whole workflow took roughly 13 minutes versus 9 minutes: the dwctl image
build grew from 4m38s to 10m21s. Faster test jobs do not establish faster overall
PR completion. These are observations from the initial PR commit, not timings
for subsequent review fixes.

See [the Rust test efficiency skill](../.claude/skills/rust-test-efficiency/SKILL.md)
for guidance when adding tests.
