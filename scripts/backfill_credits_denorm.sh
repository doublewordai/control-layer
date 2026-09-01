#!/usr/bin/env bash
#
# Backfill credits_transactions.service_tier from authoritative sources
# (COR-507, migration 112).
#
# Context: migration 112 adds service_tier; the analytics batcher sets it at
# INSERT going forward, computed from (fusillade_batch_id, completion_window).
# This script fills history.
#
# --- Why this does NOT join http_analytics per row ---
# The obvious backfill (recompute each row's tier from its matching
# http_analytics row via ha.id::text = ct.source_id) means ~48M random probes
# into the 156M-row http_analytics id-index. On the prod primary those are cold
# Neon pageserver reads (Neon/PS_ReadIO) -> ~200s per 100k rows and days total,
# and it competes with live traffic for pageserver I/O. Validated too heavy.
#
# Instead we classify from cheaper, authoritative sources (validated on a prod
# clone, see COR-507 notes):
#
#   * Batch vs async (rows WITH fusillade_batch_id, ~94% of usage rows):
#     take the SLA from fusillade.batches.completion_window, NOT http_analytics.
#     When http_analytics.batch_sla is populated it equals
#     batches.completion_window in 100% of sampled rows; and batch_sla was simply
#     not populated before ~Feb 2026, so batches.completion_window is both
#     consistent with the batcher going forward AND corrects old rows that the
#     raw analytics would mislabel `async` (real 24h batches with an empty SLA).
#       completion_window = '24h' -> batch ;  otherwise (incl. a deleted batch
#       row, which we can't resolve) -> async.
#     fusillade.batches is small and hash-joins in memory: no http_analytics.
#
#   * realtime vs flex (rows WITHOUT fusillade_batch_id, ~6%):
#     default realtime; flex is a non-batch request that carried an SLA. Its SLA
#     lives only in http_analytics.batch_sla, so flex (~0.2% of rows) is the one
#     case that needs http_analytics -- recovered by a SINGLE bounded scan
#     (Phase 2), not per-row probes. Pre-cutover flex was never recorded and is
#     correctly left realtime.
#
# Tier rule (mirrors compute_service_tier in batcher.rs):
#   batch id + '24h'      -> batch
#   batch id + otherwise  -> async
#   no batch id + SLA     -> flex
#   no batch id + no SLA  -> realtime
#
# Corrective + overwriting: this recomputes every usage row (no `service_tier
# IS NULL` guard), so it also fixes any rows written by an earlier http_analytics
# -based run. Non-usage rows (grants/purchases) are left untouched (NULL).
# Resumable via START_SEQ (set it to the last committed `seq<=` printed below).
#
# Run AFTER migration 112 is deployed (so the batcher populates new rows).
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_credits_denorm.sh
# Optional env:
#   BATCH_SIZE     seq-range width swept per batch (default 100000)
#   SLEEP_SECONDS  pause between batches (default 0.1)
#   START_SEQ      resume: skip seq <= this (default 0)
#   FLEX_CUTOFF    only scan http_analytics at/after this date for flex
#                  (default 2026-01-01; set empty to scan all history)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-100000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"
START_SEQ="${START_SEQ:-0}"
FLEX_CUTOFF="${FLEX_CUTOFF:-2026-01-01}"
HELPER_IDX=idx_credits_tx_seq_backfill

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

MAX_SEQ=$(psql_q "SELECT COALESCE(MAX(seq), 0) FROM credits_transactions;")
if [ -z "$MAX_SEQ" ] || [ "$MAX_SEQ" -eq 0 ]; then
  echo "backfill_credits_denorm: no credits_transactions rows found; nothing to do" >&2
  exit 0
fi

echo "backfill_credits_denorm: sweeping seq (${START_SEQ}, ${MAX_SEQ}]  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s"

# credits_transactions has NO standalone index on seq (PK is id; every index
# carrying seq leads with user_id), so the seq-range sweep would seq-scan the
# whole table each batch. Build a plain btree on seq first (CONCURRENTLY, never
# locks writes) and drop it when done. Drop any leftover first so an aborted
# CONCURRENTLY build can't leave an INVALID index that CREATE would keep.
echo "backfill_credits_denorm: (re)building helper index ${HELPER_IDX} CONCURRENTLY (may take a few minutes)…"
trap 'psql_q "DROP INDEX CONCURRENTLY IF EXISTS ${HELPER_IDX};" >/dev/null 2>&1 || true' EXIT INT TERM
psql_q "DROP INDEX CONCURRENTLY IF EXISTS ${HELPER_IDX};"
psql_q "CREATE INDEX CONCURRENTLY ${HELPER_IDX} ON credits_transactions (seq);"

# --- Phase 1: batch/async/realtime from fusillade.batches (no http_analytics) ---
CURSOR="$START_SEQ"
total_rows=0
batches=0
SECONDS=0
while [ "$CURSOR" -lt "$MAX_SEQ" ]; do
  hi=$(( CURSOR + BATCH_SIZE ))
  if [ "$hi" -gt "$MAX_SEQ" ]; then hi="$MAX_SEQ"; fi
  # Two UPDATEs + a count, one implicit transaction (atomic per chunk):
  #   1. default every usage row: batch id -> async, else -> realtime
  #   2. upgrade to batch where the batch's window is 24h (deleted batch rows
  #      have no batches match and stay async)
  affected=$(psql_q "
    UPDATE credits_transactions ct
       SET service_tier = CASE WHEN ct.fusillade_batch_id IS NOT NULL THEN 'async' ELSE 'realtime' END
     WHERE ct.seq > ${CURSOR} AND ct.seq <= ${hi}
           AND ct.transaction_type = 'usage';
    UPDATE credits_transactions ct
       SET service_tier = 'batch'
      FROM fusillade.batches b
     WHERE ct.fusillade_batch_id = b.id
           AND b.completion_window = '24h'
           AND ct.seq > ${CURSOR} AND ct.seq <= ${hi}
           AND ct.service_tier = 'async';
    SELECT count(*) FROM credits_transactions
     WHERE seq > ${CURSOR} AND seq <= ${hi} AND transaction_type = 'usage';")
  total_rows=$(( total_rows + affected ))
  batches=$(( batches + 1 ))
  CURSOR="$hi"
  printf '  seq<=%-12s  rows %-7s  (total %s)\n' "$hi" "$affected" "$total_rows"
  sleep "${SLEEP_SECONDS}"
done

# --- Phase 2: flex, one bounded scan of http_analytics ---
# A non-batch request that carried an SLA. Only source is http_analytics.batch_sla.
# Single set-based UPDATE: scan http_analytics once (bounded by FLEX_CUTOFF),
# upgrade the matching realtime rows to flex. Not per-row probing.
flex_where_cutoff=""
if [ -n "$FLEX_CUTOFF" ]; then flex_where_cutoff="AND ha.timestamp >= '${FLEX_CUTOFF}'"; fi
echo "backfill_credits_denorm: Phase 2 flex — scanning http_analytics ${FLEX_CUTOFF:+from ${FLEX_CUTOFF} }…"
flex_rows=$(psql_q "
  WITH f AS (
    UPDATE credits_transactions ct
       SET service_tier = 'flex'
      FROM http_analytics ha
     WHERE ha.id::text = ct.source_id
           ${flex_where_cutoff}
           AND ha.fusillade_batch_id IS NULL
           AND COALESCE(ha.batch_sla, '') <> ''
           AND ct.fusillade_batch_id IS NULL
           AND ct.service_tier = 'realtime'
    RETURNING 1
  )
  SELECT count(*) FROM f;")

elapsed=$SECONDS
echo "backfill_credits_denorm: DONE"
printf '  usage rows classified : %s\n' "$total_rows"
printf '  flex rows (Phase 2)   : %s\n' "$flex_rows"
printf '  duration              : %dm %02ds (sweep only, excl. index build)\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
printf '  batches               : %s  (BATCH_SIZE=%s, SLEEP_SECONDS=%s)\n' "$batches" "$BATCH_SIZE" "$SLEEP_SECONDS"
printf '  seq swept             : (%s, %s]\n' "$START_SEQ" "$MAX_SEQ"

echo "backfill_credits_denorm: dropping helper index ${HELPER_IDX} CONCURRENTLY…"
psql_q "DROP INDEX CONCURRENTLY IF EXISTS ${HELPER_IDX};"

echo
echo "Spot-check tier distribution:"
echo "  psql \"\$DATABASE_URL\" -c \"SELECT service_tier, count(*) FROM credits_transactions WHERE transaction_type='usage' GROUP BY 1 ORDER BY 2 DESC;\""
