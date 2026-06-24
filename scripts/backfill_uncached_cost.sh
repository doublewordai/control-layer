#!/usr/bin/env bash
#
# Backfill http_analytics.uncached_cost for historical rows (migration 105).
#
# Context: migration 105 turns total_cost from a generated list-price column into a
# batcher-written CHARGED column, and adds a plain `uncached_cost` (list price). New rows
# get uncached_cost from the app; historical rows are NULL until this runs. For every
# historical row the list price == the old total_cost (no caching existed yet), so we copy
# total_cost across.
#
# This is deliberately a side-script, not part of the migration: a single big UPDATE on a
# large http_analytics would bloat + hold one long transaction. Here we copy in bounded
# batches, each its own committed transaction, so locks/bloat stay small and replication /
# autovacuum keep up. Safe to run while the app is live.
#
# Idempotent + resumable: the predicate `uncached_cost IS NULL AND total_cost IS NOT NULL`
# only matches un-backfilled, *priced* rows — already-filled rows drop out, and unpriced
# rows (total_cost NULL, correctly NULL list price) are never touched, so reruns terminate
# cleanly with no wasted writes. Re-run any time; interrupt and resume freely.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_uncached_cost.sh
# Optional env: BATCH_SIZE (default 10000), SLEEP_SECONDS between batches (default 0.2).

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-10000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.2}"

remaining() {
  psql "$DATABASE_URL" -qtAc \
    "SELECT count(*) FROM http_analytics WHERE uncached_cost IS NULL AND total_cost IS NOT NULL;"
}

echo "backfill_uncached_cost: $(remaining) rows to fill (batch=${BATCH_SIZE})"

total=0
while :; do
  # Each batch is one transaction (one psql invocation). The UPDATE is a CTE and the
  # statement is a final SELECT count(*), so psql emits exactly one numeric row. No `grep …
  # || true` pipeline — that would swallow a psql/DB error as a clean "0 rows" and stop the
  # backfill silently; here a failed psql aborts the script under `set -e`.
  affected=$(psql "$DATABASE_URL" -qtA -c "
    WITH batch AS (
      SELECT id FROM http_analytics
      WHERE uncached_cost IS NULL AND total_cost IS NOT NULL
      LIMIT ${BATCH_SIZE}
    ), upd AS (
      UPDATE http_analytics h
         SET uncached_cost = h.total_cost
        FROM batch
       WHERE h.id = batch.id
      RETURNING 1
    )
    SELECT count(*) FROM upd;")

  if [ "${affected}" -eq 0 ]; then
    break
  fi
  total=$((total + affected))
  echo "  …filled ${total} so far (last batch ${affected})"
  sleep "${SLEEP_SECONDS}"
done

echo "backfill_uncached_cost: done — ${total} rows filled, $(remaining) remaining"
