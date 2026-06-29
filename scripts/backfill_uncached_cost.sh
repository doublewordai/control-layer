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
# Batching strategy — sweep the PRIMARY KEY (id) in fixed-width ranges. Each batch is a
# bounded index-range scan on the existing PK, so the work per batch is constant and the
# whole run is O(rows). We deliberately do NOT use the `WHERE uncached_cost IS NULL LIMIT n`
# shape: with no index on that predicate it seq-scans, and as rows fill it must scan past the
# already-done rows to find the next batch — degrading badly on a multi-GB table. The id
# sweep needs no temporary index (it rides the PK), which is why the staging and prod runs
# are identical, with no CREATE/DROP INDEX steps.
#
# Idempotent + resumable: only rows with `uncached_cost IS NULL AND total_cost IS NOT NULL`
# are written, so already-filled and unpriced rows are no-ops. Re-run any time. To resume
# after an interrupt, pass START_ID set to the last `id<=N` the run printed.
#
# Usage:
#   DATABASE_URL=postgres://...  ./scripts/backfill_uncached_cost.sh
# Optional env:
#   BATCH_SIZE     id-range width swept per batch (default 20000; ~= rows/batch on a dense table)
#   SLEEP_SECONDS  pause between batches — throttles WAL volume / replication lag (default 0.1)
#   START_ID       resume from this id (default: the table's min id)

set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL to the http_analytics database}"
BATCH_SIZE="${BATCH_SIZE:-20000}"
SLEEP_SECONDS="${SLEEP_SECONDS:-0.1}"

# `-v ON_ERROR_STOP=1` makes psql exit non-zero on a SQL error (so set -e / assignments trip),
# and `-X` ignores ~/.psqlrc so a user's config can't alter behaviour. Applied to every call.
psql_q() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -X -qtAc "$1"; }

# PK bounds — index-only, instant even on a huge table. coalesce handles an empty table
# (min=1,max=0 → the loop body never runs).
bounds=$(psql_q "SELECT coalesce(min(id),1) || ' ' || coalesce(max(id),0) FROM http_analytics")
MIN_ID="${bounds%% *}"
MAX_ID="${bounds##* }"

# First batch is the half-open range (CURSOR, CURSOR+BATCH], so start one below MIN_ID (or
# below START_ID) to include the boundary row.
CURSOR=$(( ${START_ID:-$MIN_ID} - 1 ))

echo "backfill_uncached_cost: sweeping id (${CURSOR}, ${MAX_ID}]  batch=${BATCH_SIZE}  sleep=${SLEEP_SECONDS}s"

total=0
batches=0
SECONDS=0   # bash builtin: wall-clock seconds since reset
while [ "$CURSOR" -lt "$MAX_ID" ]; do
  hi=$(( CURSOR + BATCH_SIZE ))
  # One batch = one committed transaction. The UPDATE drives off the PK range; the row count
  # comes back via a CTE + final SELECT count(*) (no grep — a psql/DB error still fails the
  # script under set -e, rather than reading as a clean "0 rows").
  affected=$(psql_q "
    WITH upd AS (
      UPDATE http_analytics
         SET uncached_cost = total_cost
       WHERE id > ${CURSOR} AND id <= ${hi}
         AND uncached_cost IS NULL
         AND total_cost IS NOT NULL
      RETURNING 1
    )
    SELECT count(*) FROM upd;")
  total=$(( total + affected ))
  batches=$(( batches + 1 ))
  CURSOR="$hi"
  printf '  id<=%-12s  filled +%-6s  (total %s)\n' "$hi" "$affected" "$total"
  sleep "${SLEEP_SECONDS}"
done

# Summary. Duration is wall-clock (it includes the inter-batch sleeps), so `rate` is the
# realistic throttled throughput — i.e. what a prod run of this size would take.
elapsed=$SECONDS
if [ "$elapsed" -gt 0 ]; then rate=$(( total / elapsed )); else rate="$total"; fi
echo "backfill_uncached_cost: DONE"
printf '  rows filled : %s\n' "$total"
printf '  duration    : %dm %02ds (wall-clock, incl. throttle sleeps)\n' $(( elapsed / 60 )) $(( elapsed % 60 ))
printf '  throughput  : ~%s rows/s\n' "$rate"
printf '  batches     : %s  (BATCH_SIZE=%s, SLEEP_SECONDS=%s)\n' "$batches" "$BATCH_SIZE" "$SLEEP_SECONDS"
printf '  id swept    : up to %s\n' "$MAX_ID"
