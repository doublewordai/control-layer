#!/usr/bin/env bash
#
# Backfill credits_transactions.fusillade_request_id from http_analytics
# (COR-524 follow-up / "Usage E", migrations 119 + 120).
#
# Purpose: the responses view (GET /admin/api/v1/batches/requests[/{id}]) reads per-request
# COST off the credits ledger by fusillade_request_id, durably (survives http_analytics
# retention). The batcher sets it at INSERT going forward; this fills history.
#
# The credit -> request link lives in http_analytics: ct.source_id is the http_analytics row
# id (as text) and http_analytics.fusillade_request_id is the value we copy onto the credit.
# Only usage credits whose http_analytics row is still present get linked; realtime
# (non-fusillade) usage and rows aged out of http_analytics stay NULL (correct). RUN THIS
# BEFORE COR-509 prunes http_analytics -- rows already gone cannot be linked.
#
# --- Why paginate over http_analytics.id WINDOWS (measured on a prod-scale Neon branch) ---
# The obvious shape -- page credits by their pk and probe http_analytics one usage-credit at a
# time -- issues ~53M RANDOM primary-key lookups into the 156M-row http_analytics table. On
# Neon those are cold pageserver fetches (~1.6ms each): a 20k page took ~32s and the full run
# projected to ~23h. Driving a single whole-table UPDATE instead is worse: it either re-seq-
# scans all 53M credits per chunk to build a hash, or -- as one whole-table transaction -- runs
# for many minutes and then loses ALL progress if the Neon backend blips (large single txns are
# both slow and fragile here).
#
# Instead we sweep contiguous http_analytics.id ranges. Each window scans http_analytics
# SEQUENTIALLY by pk (prefetched -- Neon's fast path) and updates the matching usage credits
# through the credits_transactions_source_id_unique index. hashjoin/mergejoin are disabled so
# the planner keeps that ha-range -> credits-index nested loop (otherwise it seq-scans all 53M
# credits per window). Each window is its own committed transaction: bounded, resumable, and
# safe against connection/compute blips. This removes the ~23h random-READ penalty; what's left
# is the irreducible cost of writing ~53M scattered credit rows (the table is uuid-ordered, so
# the matches land ~one per heap page). Still a multi-hour, off-peak job -- but a tractable one.
#
# --- Run the index rebuild AROUND this, not before it ---
# migration 120's partial index idx_credits_tx_fusillade_request_id (fusillade_request_id, seq
# DESC) WHERE fusillade_request_id IS NOT NULL was built CREATE INDEX CONCURRENTLY and can be
# left INVALID if that build was ever interrupted (and its IF NOT EXISTS then blocks self-heal).
# Maintaining it during the sweep also taxes every one of the ~53M writes. Preferred sequence:
#     DROP INDEX CONCURRENTLY IF EXISTS idx_credits_tx_fusillade_request_id;   -- if invalid/absent
#     ./scripts/backfill_credits_fusillade_request_id.sh                        -- this script
#     CREATE INDEX CONCURRENTLY idx_credits_tx_fusillade_request_id
#         ON credits_transactions (fusillade_request_id, seq DESC)
#         WHERE fusillade_request_id IS NOT NULL;                               -- rebuild once, valid
# Only then merge/deploy the read side (#1278). The pre-#1278 read path does not use this index,
# so dropping it for the duration is safe.
#
# Idempotent + resumable: guarded by `fusillade_request_id IS NULL`; re-run to catch rows
# inserted during the sweep. Resume from the last printed `ha.id<` via START_ID.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_credits_fusillade_request_id.sh
# Optional env:
#   WINDOW         http_analytics.id ids scanned per transaction (default 50000). Sized so even
#                  the densest id bands (~1 linked credit per ha.id) commit in ~20-25s; sparse
#                  bands just iterate faster. Each window is one committed txn.
#   SLEEP_SECONDS  pause between windows (default 0.05)
#   START_ID       resume: first http_analytics.id to scan (default 0)
#   MAX_ID         last http_analytics.id to scan (default = max(http_analytics.id))

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the credits/http_analytics database}"
WINDOW="${WINDOW:-50000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.05}"
START_ID="${START_ID:-0}"

psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

MAX_ID="${MAX_ID:-$(psql_q 'SELECT max(id) FROM http_analytics')}"

echo "backfill_credits_fusillade_request_id: ha.id windows [${START_ID}, ${MAX_ID}]  window=${WINDOW}  sleep=${SLEEP_SECONDS}s"

lo="$START_ID"
total=0
SECONDS=0
while [ "$lo" -le "$MAX_ID" ]; do
  hi=$(( lo + WINDOW ))
  # One http_analytics.id window: sequential ha pk scan, probe credits by the source_id unique
  # index (hashjoin/mergejoin off so we don't seq-scan all of credits to build a hash), copy
  # ha.fusillade_request_id onto each still-NULL usage credit. Returns rows linked in the window.
  updated=$(psql_q "
    SET enable_hashjoin = off;
    SET enable_mergejoin = off;
    WITH upd AS (
      UPDATE credits_transactions ct
         SET fusillade_request_id = ha.fusillade_request_id
        FROM http_analytics ha
       WHERE ha.id >= ${lo} AND ha.id < ${hi}
         AND ha.fusillade_request_id IS NOT NULL
         AND ct.source_id = ha.id::text
         AND ct.transaction_type = 'usage'
         AND ct.fusillade_request_id IS NULL
      RETURNING 1)
    SELECT count(*) FROM upd;")
  total=$(( total + updated ))
  printf '  ha.id<%-12s  linked +%-7s  (total %s)  [%ds]\n' "$hi" "$updated" "$total" "$SECONDS"
  lo="$hi"
  sleep "${SLEEP_SECONDS}"
done

echo "backfill_credits_fusillade_request_id: DONE"
printf '  credit rows linked : %s\n' "$total"
printf '  duration           : %dm %02ds\n' $(( SECONDS / 60 )) $(( SECONDS % 60 ))
echo "  re-run to catch rows inserted during the sweep (idempotent via fusillade_request_id IS NULL)" >&2
