---
name: fusillade-development
description: Use when changing Fusillade request state, scheduling, concurrency, retries, batch or background processing, archival, or retained-response reads in control-layer.
---

# Fusillade development

Preserve state transitions and storage contracts across workers, retries, and
content movement. Follow current workspace code rather than old standalone-crate
paths or migration plans.

## Choose the owning layer

| Change | Location |
| --- | --- |
| Shared state/types/storage contracts | `fusillade-core/` |
| PostgreSQL implementation, migrations, DB retries | `fusillade-arsenal/` |
| Daemon, HTTP execution, processors, adaptive/retry policy | `fusillade/` |
| Application dispatch preparation | `dwctl/src/inference/engine/dispatch_processor.rs` |
| Protocol translation | `dwctl/src/inference/translation/` |

## Scheduling and recovery

- Distinguish process-local counters/claim mutexes from database row exclusivity
  and application-supplied model capacities. `FOR UPDATE SKIP LOCKED` prevents
  duplicate claims; it does not itself impose a fleet-wide concurrency ceiling.
  Trace capacity configuration and updates before changing multi-pod behavior.
- Foreground request/batch loops share local coordination. Background has separate
  claim loops and foreground-headroom/due-work gates. Preserve that distinction;
  a headroom observation is not a global capacity reservation.
- Inspect the current overload and retry classifiers. Adaptive overload includes
  529, 503, and labelled concurrency-limit 429; ordinary 429/timeouts differ.
  Retryability is not simply “all 5xx.” Preserve deadlines, configured status
  additions, and success-body failure classification.
- Verify cancellation, terminal persistence, and dead/stale-daemon reclaim.
  Preparation failures must not strand requests in processing. HTTP server drain
  does not prove daemon execution/state transitions are safe.

## Retention and reads

Keep ownership, retry, cancellation, download, and ZDR behavior consistent across
live and retained/archive storage. Terminal polling first probes state by identity
on primary and avoids payload/template reads while waiting. Preserve the
materialized identity lookup before ownership filtering and handle archival
racing the lookup through retained routing.

Maintenance must stay bounded and off foreground reads, preserving schema-local
cooldown/recovery. Partition retirement needs the existing session-capable
maintenance connection, attested relations, and late-write fences. Roll out
archive-aware readers and migration prerequisites before moving content. Use the
SQLx skills for query/migration changes.

## Regression examples and sources

Test competing workers, background alongside foreground, shutdown then reclaim,
permanent preparation failure, terminal reads racing archival, archived retries,
and late writes after retirement. Use relevant package-scoped Cargo tests, then
`just lint rust` and `just test rust` with migrated test databases.

- [Fusillade overview](../../../fusillade/README.md),
  [daemon implementation](../../../fusillade/src/daemon/),
  [processor tests](../../../fusillade/tests/request_processor.rs).
- [Retention guide](../../../docs/src/conceptual-guides/request-retention-maintenance.md),
  [storage implementation](../../../fusillade-arsenal/src/postgres.rs),
  [storage regression suites](../../../fusillade-arsenal/tests/).

`docs/SLA_TO_PRIORITY_MIGRATION.md` and `docs/responses-processor-design.md`
describe historical designs, not current raw completion-window APIs or dispatch
modules. The README's “only exact 529” statement also predates today's classifier.
