#!/usr/bin/env bash
#
# Backfill credits_transactions.service_tier from http_analytics history
# (COR-507, migration 112).
#
# Context: migration 112 adds the service_tier column, and the analytics batcher
# (batch_insert_credits) sets it at INSERT going forward (computed in memory from
# fusillade_batch_id + completion_window). This script fills history by recomputing
# the same classification from the matching http_analytics row: every usage row's
# source_id IS that http_analytics row's id (batch_insert_credits pushes
# analytics_id.to_string() as source_id), so ha.id::text = ct.source_id joins them.
# http_analytics carries fusillade_batch_id and batch_sla (= completion_window).
#
# Tier rule (mirrors compute_service_tier in batcher.rs):
#   batch id + 24h -> batch;  batch id + else -> async;
#   no batch id + non-empty SLA -> flex;  otherwise -> realtime.
#
# Run AFTER migration 112 is deployed (so the batcher populates new rows) and
# BEFORE COR-514 drops idx_http_analytics_id_text — the join rides that index.
#
# Side-script, not a migration: a single UPDATE over the whole ledger would hold
# one long transaction and bloat. We sweep credits_transactions by seq in fixed
# ranges, each its own committed transaction. Idempotent + resumable: the
# `service_tier IS NULL` guard skips rows already populated by the batcher or a
# prior run. Non-usage rows (grants/purchases) never match the join and stay NULL.
#
# Helper index: credits_transactions has NO standalone index on seq (the PK is
# id; every index carrying seq leads with user_id), so an un-helped seq-range
# sweep sequential-scans the whole ~48M-row table on EVERY batch (measured ~200s
# per 20k window on staging -> days total). So this script first builds a partial
# btree index on seq (CONCURRENTLY, never locks writes) and drops it when done.
# The `WHERE service_tier IS NULL` predicate keeps it small and self-shrinking as
# rows get filled, and makes re-running already-finished windows near-instant.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_credits_denorm.sh
# Optional env:
#   BATCH_SIZE     seq-range width swept per batch (default 100000)
#   SLEEP_SECONDS  pause between batches (default 0.1)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-100000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"
HELPER_IDX=idx_credits_tx_seq_backfill

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

MAX_SEQ=$(psql_q "SELECT COALESCE(MAX(seq), 0) FROM credits_transactions;")
if [ -z "$MAX_SEQ" ] || [ "$MAX_SEQ" -eq 0 ]; then
  echo "backfill_credits_denorm: no credits_transactions rows found; nothing to do" >&2
  exit 0
fi

echo "backfill_credits_denorm: sweeping seq (0, ${MAX_SEQ}]  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s"

# Build the seq helper index up front. Drop any leftover first (a prior aborted
# CONCURRENTLY build can leave an INVALID index that CREATE ... IF NOT EXISTS
# would silently keep, sending the sweep back to full table scans). Both CREATE
# and DROP run CONCURRENTLY, each as its own autocommit statement (never inside a
# txn), so they never block live writes. Not counted in the sweep duration below.
echo "backfill_credits_denorm: (re)building helper index ${HELPER_IDX} CONCURRENTLY (may take a few minutes)…"
psql_q "DROP INDEX CONCURRENTLY IF EXISTS ${HELPER_IDX};"
psql_q "CREATE INDEX CONCURRENTLY ${HELPER_IDX} ON credits_transactions (seq) WHERE service_tier IS NULL;"

CURSOR=0
total_rows=0
batches=0
SECONDS=0
while [ "$CURSOR" -lt "$MAX_SEQ" ]; do
  hi=$(( CURSOR + BATCH_SIZE ))
  if [ "$hi" -gt "$MAX_SEQ" ]; then hi="$MAX_SEQ"; fi
  affected=$(psql_q "
    WITH upd AS (
      UPDATE credits_transactions ct
         SET service_tier = CASE
               WHEN ha.fusillade_batch_id IS NOT NULL AND ha.batch_sla = '24h' THEN 'batch'
               WHEN ha.fusillade_batch_id IS NOT NULL                          THEN 'async'
               WHEN COALESCE(ha.batch_sla, '') <> ''                           THEN 'flex'
               ELSE 'realtime'
             END
        FROM http_analytics ha
       WHERE ct.seq > ${CURSOR} AND ct.seq <= ${hi}
             AND ct.service_tier IS NULL
             AND ha.id::text = ct.source_id
      RETURNING 1
    )
    SELECT count(*) FROM upd;")
  total_rows=$(( total_rows + affected ))
  batches=$(( batches + 1 ))
  CURSOR="$hi"
  printf '  seq<=%-12s  rows +%-6s  (total %s)\n' "$hi" "$affected" "$total_rows"
  sleep "${SLEEP_SECONDS}"
done

elapsed=$SECONDS
echo "backfill_credits_denorm: DONE"
printf '  ledger rows updated : %s\n' "$total_rows"
printf '  duration            : %dm %02ds (wall-clock, incl. throttle sleeps)\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
printf '  batches             : %s  (BATCH_SIZE=%s, SLEEP_SECONDS=%s)\n' "$batches" "$BATCH_SIZE" "$SLEEP_SECONDS"
printf '  seq swept           : up to %s\n' "$MAX_SEQ"

echo "backfill_credits_denorm: dropping helper index ${HELPER_IDX} CONCURRENTLY…"
psql_q "DROP INDEX CONCURRENTLY IF EXISTS ${HELPER_IDX};"

echo
echo "Spot-check tier distribution:"
echo "  psql \"\$DATABASE_URL\" -c \"SELECT service_tier, count(*) FROM credits_transactions WHERE transaction_type='usage' GROUP BY 1 ORDER BY 2 DESC;\""
