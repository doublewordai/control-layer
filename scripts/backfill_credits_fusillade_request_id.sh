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
# join http_analytics — by ha.id = source_id::bigint, i.e. http_analytics's PRIMARY KEY (a
# cheap PK lookup), NOT the ha.id::text form the COR-507 backfill warned about (that form
# can't use an index, and idx_http_analytics_id_text is now dropped).
#
# --- Why keyset pagination on the credits PK (NOT a seq/created_at range) ---
# credits_transactions has NO standalone index on `seq` (it appears only as a trailing column
# in composite indexes), and the planner won't use the created_at index for the filtered
# range either — so a `WHERE seq/created_at BETWEEN ...` sweep SEQ-SCANS all ~28M rows PER
# BATCH (measured ~90s/pass on a prod-scale Neon branch → hours). Paginating by the primary
# key (`WHERE id > $last ORDER BY id LIMIT n`) is a single index-ordered pass of the table —
# each batch touches only its own rows, joins http_analytics + the target row by PK, and
# needs NO helper index. (Measured plan: three nested-loop PK index scans, zero seq scans.)
#
# Cost profile: this still probes/updates one row per usage credit, so at full scale it is a
# multi-hour, batched, resumable job best run off-peak on the primary — same class as the
# COR-507 credits backfill. Only usage rows whose http_analytics row is still present can be
# linked; realtime (non-fusillade) usage stays NULL (correct). RUN THIS BEFORE COR-509 prunes
# http_analytics: rows already aged out cannot be linked (nothing to read from). If full
# history is not needed, scope it to the retention window (start the sweep from a recent id).
#
# Idempotent + resumable: guarded by `fusillade_request_id IS NULL`; paginated by id, each
# batch its own committed transaction. Resume from the last printed `id<=` via START_ID.
# Re-run to 0 to catch rows inserted during the sweep.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_credits_fusillade_request_id.sh
# Optional env:
#   BATCH_SIZE     rows scanned per transaction (default 20000)
#   SLEEP_SECONDS  pause between batches (default 0.1)
#   START_ID       resume: skip ids <= this uuid (default the zero uuid = from the start)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-20000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"
START_ID="${START_ID:-00000000-0000-0000-0000-000000000000}"

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

echo "backfill_credits_fusillade_request_id: keyset by id from ${START_ID}  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s"

CURSOR="$START_ID"
total_rows=0
SECONDS=0
while :; do
  # One PK-ordered page of credits; link the usage rows in it whose http_analytics row still
  # carries a fusillade_request_id. Returns: next cursor (max id in the page), rows scanned
  # (0 = done), rows linked. The CASE guards the ::bigint cast so it only runs for numeric
  # source_ids (grant/purchase rows use a UUID) regardless of WHERE-clause reordering.
  row=$(psql_q "
    WITH b AS (
      SELECT id, source_id
      FROM credits_transactions
      WHERE id > '${CURSOR}'::uuid
      ORDER BY id
      LIMIT ${BATCH_SIZE}
    ),
    upd AS (
      UPDATE credits_transactions ct
         SET fusillade_request_id = ha.fusillade_request_id
        FROM b
        JOIN http_analytics ha
          ON ha.id = CASE WHEN b.source_id ~ '^[0-9]+\$' THEN b.source_id::bigint END
         AND ha.fusillade_request_id IS NOT NULL
       WHERE ct.id = b.id
         AND ct.transaction_type = 'usage'
         AND ct.fusillade_request_id IS NULL
      RETURNING 1
    )
    SELECT
      COALESCE((SELECT id FROM b ORDER BY id DESC LIMIT 1), '${CURSOR}'::uuid)::text,
      (SELECT count(*) FROM b),
      (SELECT count(*) FROM upd);")

  next_last="${row%%|*}"
  rest="${row#*|}"
  scanned="${rest%%|*}"
  updated="${rest##*|}"

  [ "${scanned:-0}" -eq 0 ] && break
  total_rows=$(( total_rows + updated ))
  CURSOR="$next_last"
  printf '  id<=%-38s  scanned %-7s  linked +%-6s  (total %s)\n' "$next_last" "$scanned" "$updated" "$total_rows"
  sleep "${SLEEP_SECONDS}"
done

elapsed=$SECONDS
echo "backfill_credits_fusillade_request_id: DONE"
printf '  credit rows linked : %s\n' "$total_rows"
printf '  duration           : %dm %02ds\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
echo "  re-run to catch rows inserted during the sweep (idempotent via fusillade_request_id IS NULL)" >&2
