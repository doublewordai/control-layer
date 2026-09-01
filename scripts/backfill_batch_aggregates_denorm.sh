#!/usr/bin/env bash
#
# Backfill batch_aggregates.service_tier from fusillade.batches (COR-514, migration 113).
#
# batch_aggregates is one row per batch (small). Every batch has a fusillade_batch_id,
# so its tier is `async` (1h SLA) or `batch` (24h SLA). We read the SLA from the
# AUTHORITATIVE source, fusillade.batches.completion_window ('24h' -> batch, else ->
# async), matching compute_service_tier in the batcher AND the credits backfill. We do
# NOT read http_analytics.batch_sla: it was unpopulated before ~Feb 2026, so it would
# mislabel old 24h batches as async and disagree with credits_transactions.service_tier
# for the same batch. A deleted batch (no fusillade.batches row) has no SLA to resolve
# and defaults to async, as in the credits backfill.
#
# service_tier is immutable per batch (set once), so no quiescence guard is needed and
# there is no race with the live fold (it only writes new, non-NULL rows). This is a
# single set-based UPDATE over the NULL rows — a correlated PK lookup into the small
# fusillade.batches table, seconds for the whole read model. Idempotent: guarded by
# service_tier IS NULL. Run after migration 113 + the batcher change are deployed.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_batch_aggregates_denorm.sh

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"

echo "backfill_batch_aggregates_denorm: set-based service_tier backfill"

psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X <<'SQL'
\timing on
UPDATE batch_aggregates ba
   SET service_tier = CASE
         WHEN (SELECT b.completion_window
                 FROM fusillade.batches b
                WHERE b.id = ba.fusillade_batch_id) = '24h'
         THEN 'batch' ELSE 'async' END
 WHERE ba.service_tier IS NULL;
SQL

echo "backfill_batch_aggregates_denorm: DONE"
