# Request retention and database maintenance

Archival removes terminal requests from the active `requests` table while preserving
results in retained storage. The active table can shrink sharply even while new
requests keep arriving. Its physical indexes may remain large, and inaccurate
statistics must not turn response polling into repeated broad index scans.

## Reading a waiting response

`PostgresRequestManager::get_terminal_request_detail` probes only the request state
by primary identity on the primary database. Running requests return no detail;
terminal requests load the existing detail representation, including retained
routing and ownership checks. If archival removes the live row between these reads,
the existing reader resolves the retained route. ZDR decryption still happens in
the caller after the terminal detail is retrieved.

The full-detail query materializes its ID lookup before applying the ownership
filter. Keep that boundary: moving `created_by IS NOT NULL` into the lookup makes
the partial user/sort index eligible again. Do not fetch templates or payloads on
every waiting poll. Sequential scans can still be appropriate for tiny tables;
the invariant is avoiding a broad partial user-index scan for one identity.

## Automatic maintenance

The migration sets request auto-analyze to an absolute 1,000-change threshold and
vacuum/insert-vacuum thresholds to 5,000. These are eligibility thresholds, not
execution deadlines. Autovacuum remains responsible for reclaiming reusable space.

Batch and batchless archive passes also check a schema-local maintenance row.
All workers share a one-minute cooldown, committed before work begins. A check
refreshes statistics after 1,000 changes, or after five minutes when there are
unanalysed changes. Fresh statistics need no analyze. An active vacuum is allowed
to finish first.

Maintenance uses the primary pool with the same transaction-local schema routing
as other storage operations. Lock waits are capped at 250 ms and the analyze
statement at 20 seconds. An unfinished attempt pauses subsequent archive passes
until its one-minute cooldown expires, including when a worker is cancelled or
restarted. Workers allow up to 25 seconds for an in-progress check, waiting
without retaining a connection between probes. The daemon's existing retry loop
then retries automatically. Foreground
requests do not run or wait for this maintenance check. An already running archive
transaction is allowed to complete.

`fusillade_request_statistics_maintenance_total{outcome}` records `analyzed`,
`deferred`, and `error`. Repeated errors/deferred passes, old table statistics with
continuing changes, growing dead rows, and pool-acquisition latency need operational
alerts. A sustained maintenance failure delays archival; it must not be ignored as
healthy retention progress.

## Deployment and verification

Before upgrading a populated database, satisfy any earlier migration prerequisites
in the release. In particular, the preceding retained-response created-index
migration requires `scripts/prepare_retained_created_index.sql` to have completed;
it fails startup safely if that index is missing or incomplete. That online index
preparation is separate from the statistics-maintenance change.

Apply the additive migration through the normal application migration path, then
roll out the application. Old and new replicas can coexist; no table rewrite,
blocking index rebuild, or scale-to-zero step is required. The runtime database role
must have the requests table owner role’s permissions and be able to update the maintenance row.
Existing installations where the runtime role owns the requests table meet this requirement.

Verify automatic analyze timestamps advance under movement, maintenance errors do
not accumulate, retention continues making progress, and request latency stays
stable. An application rollback can leave the migration in place. The down migration
restores the previous maintenance settings and removes the coordination row; run
it only after all new binaries have stopped using it.

Tests cover a mostly drained table with misleading partial-index statistics,
status polling while template access is blocked, reads during atomic archival,
maintenance cooldown/crash recovery, and retry after an analyze lock conflict.
These tests run in the existing Arsenal Rust test suite. No manual analyze is
required in the deployment sequence. The migration needs a brief metadata lock;
its one-second lock timeout lets a busy deployment retry instead of waiting
indefinitely in the database lock queue.

This does not shrink existing table/index files, guarantee a particular CPU budget,
or replace database-capacity monitoring. Existing excess storage requires a
separate assessed online maintenance operation.
