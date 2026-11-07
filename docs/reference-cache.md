# Reference-cache database limits

The reference cache is optional shadow telemetry. It estimates reusable prompt
tokens in `http_analytics.reference_cached_tokens`; it does not set prices, bill
requests, or change the usage sent to clients. A failed or skipped estimate is
recorded as NULL. `reference_cache.enabled: false` disables the feature.

## Lock ordering

All operations lock prefix rows in primary-key order. Upserts sort their input
by `prefix_hash` within their fixed principal/model scope. Expiry refreshes
explicitly lock eligible rows in that order before updating them; ordering an
UPDATE's input alone does not guarantee row-lock ordering. Fresh rows are read
without locking. Cleanup orders its victims by the full primary key and uses
`SKIP LOCKED` to avoid waiting for rows already in use.

## Connection and work budgets

Each reference-cache instance has its own lazy PostgreSQL pool, capped by
`reference_cache.max_connections` (default four). Principal resolution, lookups,
first-token writes and completion writes share that pool. Cleanup has a separate
one-connection pool per application replica. Include these connections in the
database budget across replicas and overlapping rollout generations.

Prefix operations and cleanup also share four transaction-scoped advisory slots
across the entire database. A busy slot is tried without waiting; when all four
are occupied the operation is skipped. Adding replicas therefore cannot multiply
concurrent prefix queries. Principal resolution is a memoised indexed key lookup
on the dedicated pool, with the same server deadlines, but does not take a prefix
slot.

These pools inherit the current primary's connection options, including schema
and authentication settings, and follow runtime primary replacements. They do
not use a replica because reference-cache entries are unlogged. Old cache pools
are closed when the source pool changes.

Pool acquisition waits at most 500 ms. Every cache operation enforces a one-second
PostgreSQL statement timeout and a 250 ms lock timeout inside its transaction.
Transaction-local settings work through transaction-mode poolers without changing
the settings of their next client. These server deadlines
also stop work whose caller has timed out and dropped its Rust future. The
dedicated pool, rather than a caller-owned semaphore permit, enforces connection
capacity while abandoned SQL finishes. These settings never change main-pool
sessions.

Lookups and writes accept at most 1,024 prefix boundaries per operation. Larger
requests skip the estimate instead of submitting unbounded arrays. Each cleanup
tick processes at most ten batches of 10,000 expired rows; errors end that tick.

This isolates application-pool capacity and bounds individual cache operations.
CPU, memory, storage and the database server are still shared. Monitor aggregate
cache query time, lock waits, deadlocks and table maintenance alongside estimate
errors/timeouts. Disable the shadow feature if its resource cost remains too
high. Complete resource isolation would require a separate cache datastore;
increasing the connection cap is not a substitute for fixing contention.
