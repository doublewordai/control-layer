#!/usr/bin/env bash
#
# Backfill batch_aggregates.service_tier from http_analytics history
# (COR-514, migration 113).
#
# batch_aggregates is one row per batch (small). Every batch has a fusillade_batch_id,
# so its tier is `async` (1h SLA) or `batch` (24h SLA) — read the SLA from any
# http_analytics row of that batch (constant per batch). The per-batch lookup rides
# idx_analytics_fusillade_batch_id, so run this BEFORE that index is dropped and after
# migration 113 + the batcher change are deployed (new batches are set at fold time).
#
# Idempotent + resumable: per-row UPDATE guarded by `service_tier IS NULL`, swept by
# max_seq in fixed ranges, each its own committed transaction.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_batch_aggregates_denorm.sh
# Optional env:
#   BATCH_SIZE     max_seq-range width swept per batch (default 50000)
#   SLEEP_SECONDS  pause between batches (default 0.1)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-50000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

MAX_SEQ=$(psql_q "SELECT COALESCE(MAX(max_seq), 0) FROM batch_aggregates;")
if [ -z "$MAX_SEQ" ] || [ "$MAX_SEQ" -eq 0 ]; then
  echo "backfill_batch_aggregates_denorm: no batch_aggregates rows; nothing to do" >&2
  exit 0
fi

echo "backfill_batch_aggregates_denorm: sweeping max_seq (0, ${MAX_SEQ}]  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s"

CURSOR=0
total_rows=0
batches=0
SECONDS=0
while [ "$CURSOR" -lt "$MAX_SEQ" ]; do
  hi=$(( CURSOR + BATCH_SIZE ))
  if [ "$hi" -gt "$MAX_SEQ" ]; then hi="$MAX_SEQ"; fi
  affected=$(psql_q "
    WITH upd AS (
      UPDATE batch_aggregates ba
         SET service_tier = CASE WHEN s.batch_sla = '24h' THEN 'batch' ELSE 'async' END
        FROM LATERAL (
               SELECT batch_sla
               FROM http_analytics ha
               WHERE ha.fusillade_batch_id = ba.fusillade_batch_id
               LIMIT 1
             ) s
       WHERE ba.max_seq > ${CURSOR} AND ba.max_seq <= ${hi}
             AND ba.service_tier IS NULL
      RETURNING 1
    )
    SELECT count(*) FROM upd;")
  total_rows=$(( total_rows + affected ))
  batches=$(( batches + 1 ))
  CURSOR="$hi"
  printf '  max_seq<=%-12s  rows +%-6s  (total %s)\n' "$hi" "$affected" "$total_rows"
  sleep "${SLEEP_SECONDS}"
done

elapsed=$SECONDS
echo "backfill_batch_aggregates_denorm: DONE"
printf '  batch rows updated : %s\n' "$total_rows"
printf '  duration           : %dm %02ds\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
printf '  batches            : %s  (BATCH_SIZE=%s, SLEEP_SECONDS=%s)\n' "$batches" "$BATCH_SIZE" "$SLEEP_SECONDS"
