#!/usr/bin/env bash
#
# Backfill credits_transactions.fusillade_request_id from http_analytics
# (COR-524 follow-up / "Usage E", migration 119).
#
# Purpose: the responses view (GET /admin/api/v1/batches/requests[/{id}]) reads per-request
# COST off the credits ledger by fusillade_request_id, durably (survives http_analytics
# retention). The batcher sets it at INSERT going forward; this fills history.
#
# The credit -> request link lives only in http_analytics: ct.source_id is the http_analytics
# row id (as text), and http_analytics.fusillade_request_id is the value we want. So this DOES
# join http_analytics — but by ha.id = ct.source_id::bigint, i.e. http_analytics's PRIMARY KEY
# (a cheap PK lookup), NOT the ha.id::text expression the COR-507 backfill warned about (that
# form can't use an index, and idx_http_analytics_id_text is now dropped anyway).
#
# Only usage rows whose http_analytics row is still present can be linked; realtime (non-
# fusillade) usage stays NULL (correct — no request id). RUN THIS BEFORE COR-509 prunes
# http_analytics: rows already aged out cannot be linked (nothing to read from).
#
# Idempotent + resumable: guarded by `fusillade_request_id IS NULL`, swept by `seq` in fixed
# ranges, each its own committed transaction. Re-run until it reports 0 rows.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_credits_fusillade_request_id.sh
# Optional env:
#   BATCH_SIZE     seq-range width swept per transaction (default 100000)
#   SLEEP_SECONDS  pause between ranges (default 0.1)
#   START_SEQ      resume: skip seq <= this (default 0)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-100000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"
START_SEQ="${START_SEQ:-0}"

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

MAX_SEQ=$(psql_q "SELECT COALESCE(MAX(seq), 0) FROM credits_transactions;")
if [ -z "$MAX_SEQ" ] || [ "$MAX_SEQ" -eq 0 ]; then
  echo "backfill_credits_fusillade_request_id: no credits_transactions rows; nothing to do" >&2
  exit 0
fi

echo "backfill_credits_fusillade_request_id: sweeping seq (${START_SEQ}, ${MAX_SEQ}]  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s"

CURSOR="$START_SEQ"
total_rows=0
SECONDS=0
while [ "$CURSOR" -lt "$MAX_SEQ" ]; do
  hi=$(( CURSOR + BATCH_SIZE ))
  if [ "$hi" -gt "$MAX_SEQ" ]; then hi="$MAX_SEQ"; fi
  affected=$(psql_q "
    WITH upd AS (
      UPDATE credits_transactions ct
         SET fusillade_request_id = ha.fusillade_request_id
        FROM http_analytics ha
       WHERE ha.id = ct.source_id::bigint          -- http_analytics PK lookup (cheap)
         AND ct.seq > ${CURSOR} AND ct.seq <= ${hi}
         AND ct.fusillade_request_id IS NULL
         AND ct.transaction_type = 'usage'
         AND ct.source_id ~ '^[0-9]+\$'            -- guard the ::bigint cast (grants use a UUID)
         AND ha.fusillade_request_id IS NOT NULL
      RETURNING 1
    )
    SELECT count(*) FROM upd;")
  total_rows=$(( total_rows + affected ))
  CURSOR="$hi"
  printf '  seq<=%-14s  rows +%-7s  (total %s)\n' "$hi" "$affected" "$total_rows"
  sleep "${SLEEP_SECONDS}"
done

elapsed=$SECONDS
echo "backfill_credits_fusillade_request_id: DONE"
printf '  credit rows linked : %s\n' "$total_rows"
printf '  duration           : %dm %02ds\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
echo "  re-run until 'rows +0' throughout to catch rows inserted during the sweep" >&2
