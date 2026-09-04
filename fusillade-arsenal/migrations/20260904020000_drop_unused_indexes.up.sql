-- Drop twelve indexes that are unused or strictly redundant (COR-651).
--
-- From the index audit in COR-537. Every verdict below is backed by BOTH a code
-- reading (no query needs the index, or an equivalent index serves the same
-- shape) and production `pg_stat_user_indexes` from the PRIMARY over a 35-day
-- window.
--
-- Reclaims roughly 8.6 GB, and takes `requests` from 31 indexes to 27 and
-- `request_templates` from 7 to 5 — the point being write amplification and
-- vacuum cost, not the disk. Vacuum on `requests` scales with its index count
-- and its lag was itself a finding in COR-537.
--
-- ---------------------------------------------------------------------------
-- Truly unused: no read path in the code, no scans in production
-- ---------------------------------------------------------------------------
--
--   idx_request_templates_custom_id  5149 MB       0 scans
--   idx_request_templates_model      1403 MB       6 scans
--
--     `custom_id` and `model` on request_templates appear only in INSERT column
--     lists and in test helpers; no production query filters or orders by
--     either. This is the largest single win in the audit, and not because of
--     the 6.5 GB: request_templates holds ~135M rows and is the hottest insert
--     path in the system, so both indexes were being maintained on every
--     template insert for nothing.
--
--   idx_files_name_uploaded_by        211 MB       0 scans
--
--     Orphan of the filename-uniqueness constraint that was removed. Nothing
--     looks files up by (name, uploaded_by) any more.
--
--   idx_files_status                   22 MB       0 scans
--
--     The only query mentioning files.status keys on the primary key
--     (`WHERE id = $1 AND deleted_at IS NOT NULL AND status = 'deleted'`), so
--     status is a residual filter on a single-row fetch. The purpose-based
--     lookup is served by idx_files_unfinalized_batch_files.
--
--   idx_batches_active_expiration_with_id  90 MB   0 scans
--
--     Superseded by idx_batches_claimable_expiration_with_id, formalized in the
--     migration immediately before this one. Same key and INCLUDE, but the
--     successor also excludes terminal batches, which makes it 280x smaller
--     (320 kB vs 90 MB) and is why the planner abandoned this one entirely.
--
--   idx_daemons_created_at, idx_daemons_heartbeat, idx_daemons_status
--                                     ~2.8 MB      0 scans each
--
--     `daemons` is small enough that the planner seq-scans it for every access,
--     including the two queries that do filter on status (`WHERE d.status =
--     'dead'` and the optional-status listing ordered by created_at). Note the
--     heartbeat index was being maintained on every heartbeat UPDATE, which is
--     the only one of the three with a meaningful write cost.
--
-- ---------------------------------------------------------------------------
-- Redundant: actively scanned, but an existing index serves the same lookups
-- ---------------------------------------------------------------------------
--
--   idx_requests_batch_id             618 MB   3,899,872 scans
--
--     Strict prefix of idx_requests_batch_state (batch_id, state), which is
--     both SMALLER (398 MB) and far hotter (88.9M scans). Prefix lookups on
--     batch_id alone move there at no cost.
--
--   idx_requests_created_at          1111 MB         460 scans
--
--     Prefix of idx_requests_created_tier (created_at DESC, id DESC,
--     service_tier). The successor is larger (3.3 GB), so the ~13 scans/day
--     that move there get marginally more expensive; that is a good trade for
--     1.1 GB and one less index maintained on every request insert.
--
--   idx_batches_output_file_id         23 MB   4,270,500 scans
--   idx_batches_error_file_id          23 MB   3,846,908 scans
--
--     CORRECTION to the audit, which recorded these as unused duplicates of the
--     unique constraints. The opposite is true: these partial indexes take all
--     the traffic precisely BECAUSE they are smaller, and the unique constraint
--     indexes (batches_output_file_id_key 33 MB, batches_error_file_id_key
--     32 MB) sit at 0 scans. They are still redundant — the unique indexes
--     cannot be dropped, since they enforce the constraints, and they serve the
--     same equality lookups. Dropping the partials moves ~8M scans/35 days onto
--     a ~40% larger index on an 837k-row table, which is negligible, and saves
--     two index writes per batch insert plus every UPDATE that sets these
--     columns (20,767 such statements in the window).
--
-- ---------------------------------------------------------------------------
-- Deliberately NOT dropped
-- ---------------------------------------------------------------------------
--
--   idx_batches_created_by — subsumed by the created_by-led index in COR-650;
--   drop it there, once that index exists.
--
--   idx_batches_completion_window — the audit listed it as droppable. Query text
--   later showed its real consumer is the SLA missed-batches monitor
--   (~13.8k runs / 9 days). It stays until that query is re-homed in COR-656.
--
--   The seven "uncertain" indexes (COR-657) — the stats window straddles the
--   late-August demand-index deploys, so they need a snapshot delta first.
--
-- ---------------------------------------------------------------------------
-- Production deploy
-- ---------------------------------------------------------------------------
--
-- Run these CONCURRENTLY before deploying so the statements below are no-ops. A
-- plain DROP INDEX takes a brief ACCESS EXCLUSIVE lock on the table, which
-- queues behind the longest-running query — and on `requests` the claim
-- statements run for tens of seconds under load, with everything else then
-- queuing behind the waiting DROP:
--
--   DROP INDEX CONCURRENTLY IF EXISTS idx_request_templates_custom_id;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_request_templates_model;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_requests_created_at;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_requests_batch_id;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_files_name_uploaded_by;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_files_status;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_batches_active_expiration_with_id;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_batches_output_file_id;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_batches_error_file_id;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_daemons_created_at;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_daemons_heartbeat;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_daemons_status;
--
-- Suggested order: the zero-scan ones first (files, daemons, batches
-- expiration), then the request_templates pair for the payoff, then the three
-- redundant-but-hot ones last, re-checking pg_stat_user_indexes between steps.

DROP INDEX IF EXISTS idx_request_templates_custom_id;
DROP INDEX IF EXISTS idx_request_templates_model;

DROP INDEX IF EXISTS idx_requests_created_at;
DROP INDEX IF EXISTS idx_requests_batch_id;

DROP INDEX IF EXISTS idx_files_name_uploaded_by;
DROP INDEX IF EXISTS idx_files_status;

DROP INDEX IF EXISTS idx_batches_active_expiration_with_id;
DROP INDEX IF EXISTS idx_batches_output_file_id;
DROP INDEX IF EXISTS idx_batches_error_file_id;

DROP INDEX IF EXISTS idx_daemons_created_at;
DROP INDEX IF EXISTS idx_daemons_heartbeat;
DROP INDEX IF EXISTS idx_daemons_status;
